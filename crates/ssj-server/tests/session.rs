use std::ffi::OsString;
use std::time::Duration;

use ssj_proto::{
    ClientFrame, Env, EnvFilter, ErrorCode, ExitStatus, Hello, ServerFrame, SessionId, WindowSize,
    decode_server,
};
use ssj_server::backend::Direct;
use ssj_server::session::{
    Connection, Session, SessionConfig, SessionEnd, SessionEvent, SpawnTemplate,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream, ReadHalf, WriteHalf};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::timeout;

fn size() -> WindowSize {
    WindowSize::new(24, 80).unwrap()
}

fn config(script: &str) -> SessionConfig {
    SessionConfig {
        id: SessionId::parse("abcdef012345").unwrap(),
        template: SpawnTemplate {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
            env: vec![(OsString::from("PATH"), OsString::from("/usr/bin:/bin"))],
            cwd: None,
        },
        accept_env: EnvFilter::default_patterns(),
        replay_capacity: 64 * 1024,
        detached_timeout: None,
        hello_timeout: Duration::from_secs(2),
    }
}

struct Harness {
    conns: mpsc::Sender<Connection>,
    session: JoinHandle<SessionEnd>,
}

fn start(config: SessionConfig) -> Harness {
    let (conns, rx) = mpsc::channel(4);
    let session = tokio::spawn(Session::new(Direct, config).run(rx));
    Harness { conns, session }
}

struct Client {
    reader: ReadHalf<DuplexStream>,
    writer: WriteHalf<DuplexStream>,
    buf: Vec<u8>,
}

#[derive(Default, Debug)]
struct Seen {
    output: Vec<u8>,
    input_acks: Vec<u64>,
    exit: Option<ExitStatus>,
    error: Option<ErrorCode>,
    welcome: Option<ssj_proto::Welcome>,
    closed: bool,
}

impl Harness {
    async fn connect(&self) -> Client {
        let (client_end, server_end) = tokio::io::duplex(64 * 1024);
        let (server_reader, server_writer) = tokio::io::split(server_end);
        self.conns
            .send(Connection {
                reader: Box::new(server_reader),
                writer: Box::new(server_writer),
            })
            .await
            .unwrap();
        let (reader, writer) = tokio::io::split(client_end);
        Client {
            reader,
            writer,
            buf: Vec::new(),
        }
    }

    async fn end(self) -> SessionEnd {
        timeout(Duration::from_secs(10), self.session)
            .await
            .unwrap()
            .unwrap()
    }
}

impl Client {
    async fn send(&mut self, frame: ClientFrame) {
        let mut bytes = Vec::new();
        frame.encode(&mut bytes).unwrap();
        self.writer.write_all(&bytes).await.unwrap();
    }

    async fn send_raw(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).await.unwrap();
    }

    /// Next frame, or `None` when the server closed the connection.
    async fn next(&mut self) -> Option<ServerFrame> {
        loop {
            if let Some((frame, used)) = decode_server(&self.buf).expect("server sent a bad frame")
            {
                self.buf.drain(..used);
                return Some(frame);
            }
            let mut chunk = [0u8; 8192];
            match timeout(Duration::from_secs(10), self.reader.read(&mut chunk))
                .await
                .expect("timed out")
            {
                Ok(0) | Err(_) => return None,
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
            }
        }
    }

    /// Reads frames until `done` is true for what was seen so far or the connection closes.
    async fn read_until(&mut self, mut done: impl FnMut(&Seen) -> bool) -> Seen {
        let mut seen = Seen::default();
        while !done(&seen) {
            match self.next().await {
                None => {
                    seen.closed = true;
                    break;
                }
                Some(ServerFrame::Output(data)) => seen.output.extend_from_slice(&data),
                Some(ServerFrame::InputAck(offset)) => seen.input_acks.push(offset),
                Some(ServerFrame::Exit(status)) => seen.exit = Some(status),
                Some(ServerFrame::Error(code)) => seen.error = Some(code),
                Some(ServerFrame::Welcome(welcome)) => seen.welcome = Some(welcome),
            }
        }
        seen
    }

    async fn start(&mut self) -> ssj_proto::Welcome {
        self.start_with(Env::default()).await
    }

    async fn start_with(&mut self, env: Env) -> ssj_proto::Welcome {
        self.send(ClientFrame::Hello(Hello::Start { size: size(), env }))
            .await;
        self.read_until(|s| s.welcome.is_some())
            .await
            .welcome
            .expect("welcome")
    }
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|w| w == needle.as_bytes())
}

#[tokio::test]
async fn shell_input_output_and_exit_code_travel_through_the_session() {
    let harness = start(config("read x; echo got:$x; exit 4"));
    let mut client = harness.connect().await;
    let welcome = client.start().await;
    assert_eq!((welcome.input_offset, welcome.output_offset), (0, 0));

    client.send(ClientFrame::Input(b"hello\n".to_vec())).await;
    let seen = client.read_until(|s| s.exit.is_some()).await;

    assert!(
        contains(&seen.output, "got:hello"),
        "{:?}",
        String::from_utf8_lossy(&seen.output)
    );
    assert_eq!(seen.input_acks.last(), Some(&6));
    assert_eq!(seen.exit, Some(ExitStatus::Code(4)));

    client.send(ClientFrame::Close).await;
    assert!(
        matches!(harness.end().await, SessionEnd::ShellExited(status) if status.code() == Some(4))
    );
}

#[tokio::test]
async fn resuming_after_a_lost_connection_has_no_gap_and_no_duplicate() {
    let script = "i=0; while [ $i -lt 3000 ]; do echo line$i; i=$((i+1)); done; sleep 1";
    let expected: Vec<u8> = (0..3000)
        .flat_map(|i| format!("line{i}\r\n").into_bytes())
        .collect();
    let harness = start(config(script));

    let mut first = harness.connect().await;
    first.start().await;
    let part = first.read_until(|s| s.output.len() >= 5000).await;
    let seen = part.output.len() as u64;
    assert!(seen < expected.len() as u64);
    drop(first);

    let mut second = harness.connect().await;
    second
        .send(ClientFrame::Hello(Hello::Resume {
            size: size(),
            output_offset: seen,
        }))
        .await;
    let rest = second.read_until(|s| s.exit.is_some()).await;

    assert_eq!(rest.welcome.unwrap().output_offset, seen);
    assert_eq!([part.output, rest.output].concat(), expected);
}

// A program that echoes while it receives a large paste: if reading output and writing input
// ever waited for each other, this would hang.
#[tokio::test]
async fn large_paste_into_a_chatty_program_does_not_deadlock() {
    let mut cfg = config("cat");
    cfg.replay_capacity = 16 * 1024;
    let harness = start(cfg);
    let mut client = harness.connect().await;
    client.start().await;

    const LINES: usize = 12_000;
    let line = format!("{}\n", "x".repeat(79));
    let total_input = (LINES * line.len()) as u64;

    let Client {
        mut reader,
        writer,
        mut buf,
    } = client;
    let writer = std::sync::Arc::new(tokio::sync::Mutex::new(writer));
    let paste_writer = writer.clone();
    let sender = tokio::spawn(async move {
        for _ in 0..LINES / 400 {
            let mut bytes = Vec::new();
            ClientFrame::Input(line.repeat(400).into_bytes())
                .encode(&mut bytes)
                .unwrap();
            paste_writer.lock().await.write_all(&bytes).await.unwrap();
        }
    });

    // read and acknowledge output until the server has consumed the whole paste
    let (mut received, mut acked_input) = (0u64, 0u64);
    timeout(Duration::from_secs(60), async {
        let mut chunk = [0u8; 16 * 1024];
        while acked_input < total_input {
            let n = reader.read(&mut chunk).await.unwrap();
            assert!(n > 0, "server closed the connection");
            buf.extend_from_slice(&chunk[..n]);
            while let Some((frame, used)) = decode_server(&buf).unwrap() {
                buf.drain(..used);
                match frame {
                    ServerFrame::Output(data) => received += data.len() as u64,
                    ServerFrame::InputAck(offset) => acked_input = offset,
                    _ => {}
                }
            }
            let mut bytes = Vec::new();
            ClientFrame::OutputAck(received).encode(&mut bytes).unwrap();
            writer.lock().await.write_all(&bytes).await.unwrap();
        }
    })
    .await
    .expect("the session stopped moving data");
    sender.await.unwrap();

    assert_eq!(acked_input, total_input);
    assert!(
        received > total_input,
        "the shell echoed less than it was given"
    );
}

#[tokio::test]
async fn new_connection_takes_over_from_the_old_one() {
    let harness = start(config("cat"));
    let mut old = harness.connect().await;
    old.start().await;

    let mut new = harness.connect().await;
    new.send(ClientFrame::Hello(Hello::Attach { size: size() }))
        .await;
    new.read_until(|s| s.welcome.is_some()).await;

    // the old connection is closed, the new one drives the same shell
    let old_end = old.read_until(|_| false).await;
    assert!(old_end.closed);
    new.send(ClientFrame::Input(b"ping\n".to_vec())).await;
    let seen = new.read_until(|s| contains(&s.output, "ping")).await;
    assert!(contains(&seen.output, "ping"));
}

#[tokio::test]
async fn broken_clients_lose_their_connection_but_not_the_session() {
    let harness = start(config("cat"));
    let mut owner = harness.connect().await;
    owner.start().await;
    drop(owner);

    let hello = ClientFrame::Hello(Hello::Attach { size: size() });
    let mut oversized = 0xffff_ffffu32.to_be_bytes().to_vec();
    oversized.push(0x02);
    let cases: Vec<(&str, Vec<ClientFrame>, Vec<u8>, ErrorCode)> = vec![
        (
            "garbage before hello",
            vec![],
            b"\xff\xff\xff\xff garbage".to_vec(),
            ErrorCode::BadHello,
        ),
        (
            "input before hello",
            vec![ClientFrame::Input(b"x".to_vec())],
            vec![],
            ErrorCode::BadHello,
        ),
        (
            "second hello",
            vec![hello.clone(), hello.clone()],
            vec![],
            ErrorCode::Violation,
        ),
        (
            "ack of unsent output",
            vec![hello.clone(), ClientFrame::OutputAck(u64::MAX)],
            vec![],
            ErrorCode::Violation,
        ),
        (
            "oversized frame",
            vec![hello.clone()],
            oversized,
            ErrorCode::Violation,
        ),
    ];
    for (name, frames, raw, code) in cases {
        let mut bad = harness.connect().await;
        for frame in frames {
            bad.send(frame).await;
        }
        bad.send_raw(&raw).await;
        let seen = bad.read_until(|_| false).await;
        assert_eq!(seen.error, Some(code), "{name}");
        assert!(seen.closed, "{name}");
    }

    let mut good = harness.connect().await;
    good.send(ClientFrame::Hello(Hello::Attach { size: size() }))
        .await;
    good.send(ClientFrame::Input(b"still alive\n".to_vec()))
        .await;
    let seen = good
        .read_until(|s| contains(&s.output, "still alive"))
        .await;
    assert!(contains(&seen.output, "still alive"));
}

#[tokio::test]
async fn hello_must_arrive_in_time() {
    let mut cfg = config("cat");
    cfg.hello_timeout = Duration::from_millis(300);
    let harness = start(cfg);
    let mut silent = harness.connect().await;

    let seen = silent.read_until(|_| false).await;

    assert_eq!(seen.error, Some(ErrorCode::BadHello));
    assert!(seen.closed);
}

#[tokio::test]
async fn only_accepted_client_variables_reach_the_shell() {
    let harness = start(config("env"));
    let mut client = harness.connect().await;
    let env = Env::new(vec![
        ("TERM".into(), "xterm-test".into()),
        ("LD_PRELOAD".into(), "/tmp/evil.so".into()),
        ("FOO".into(), "bar".into()),
    ])
    .unwrap();
    client.start_with(env).await;

    let seen = client.read_until(|s| s.exit.is_some()).await;
    let text = String::from_utf8_lossy(&seen.output);

    assert!(text.contains("TERM=xterm-test"), "{text}");
    assert!(!text.contains("LD_PRELOAD"), "{text}");
    assert!(!text.contains("FOO=bar"), "{text}");
}

#[tokio::test]
async fn exit_while_disconnected_is_reported_after_the_client_returns() {
    let harness = start(config("printf done; exit 5"));
    let mut first = harness.connect().await;
    first.start().await;
    drop(first);
    tokio::time::sleep(Duration::from_millis(500)).await;

    let mut second = harness.connect().await;
    second
        .send(ClientFrame::Hello(Hello::Resume {
            size: size(),
            output_offset: 0,
        }))
        .await;
    let seen = second.read_until(|s| s.exit.is_some()).await;

    assert!(contains(&seen.output, "done"));
    assert_eq!(seen.exit, Some(ExitStatus::Code(5)));
    second.send(ClientFrame::Close).await;
    assert!(
        matches!(harness.end().await, SessionEnd::ShellExited(status) if status.code() == Some(5))
    );
}

// A client that never acknowledges must not make the server buffer without limit.
#[tokio::test]
async fn unacknowledged_output_is_capped_by_the_replay_buffer() {
    let mut cfg = config("yes");
    cfg.replay_capacity = 8 * 1024;
    let harness = start(cfg);
    let mut client = harness.connect().await;
    client.start().await;

    let mut total = 0usize;
    let _ = timeout(Duration::from_secs(1), async {
        loop {
            match client.next().await {
                Some(ServerFrame::Output(data)) => total += data.len(),
                Some(_) => {}
                None => break,
            }
        }
    })
    .await;

    // fixed numbers on purpose: the cap must not move together with the configured capacity
    assert!(
        total <= 8 * 1024,
        "{total} bytes arrived without any acknowledgement"
    );
    assert!(total >= 4 * 1024, "only {total} bytes arrived");
}

#[tokio::test]
async fn detached_session_ends_after_the_timeout() {
    let mut cfg = config("cat");
    cfg.detached_timeout = Some(Duration::from_millis(300));

    // never connected
    let harness = start(cfg.clone());
    assert!(matches!(harness.end().await, SessionEnd::DetachedTimeout));

    // connected, then lost
    let harness = start(cfg);
    let mut client = harness.connect().await;
    client.start().await;
    drop(client);
    assert!(matches!(harness.end().await, SessionEnd::DetachedTimeout));
}

#[tokio::test]
async fn deliberate_detach_disables_the_timeout_until_the_next_connection() {
    let mut cfg = config("cat");
    cfg.detached_timeout = Some(Duration::from_millis(300));
    let (events_tx, mut events) = mpsc::unbounded_channel();
    let (conns, rx) = mpsc::channel(4);
    let mut session = tokio::spawn(Session::new(Direct, cfg).with_events(events_tx).run(rx));
    let harness = Harness {
        conns,
        session: tokio::spawn(async { SessionEnd::Closed }),
    };

    let mut client = harness.connect().await;
    client.start().await;
    client.send(ClientFrame::Detach).await;
    assert_eq!(
        timeout(Duration::from_secs(5), events.recv())
            .await
            .unwrap(),
        Some(SessionEvent::Detached)
    );

    // several times the timeout later the session still runs
    assert!(
        timeout(Duration::from_millis(1200), &mut session)
            .await
            .is_err()
    );

    // a new client attaches and uses the same shell
    let mut again = harness.connect().await;
    again
        .send(ClientFrame::Hello(Hello::Attach { size: size() }))
        .await;
    again.send(ClientFrame::Input(b"back\n".to_vec())).await;
    again.read_until(|s| contains(&s.output, "back")).await;

    // an involuntary loss brings the timeout back
    drop(again);
    let end = timeout(Duration::from_secs(5), session)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(end, SessionEnd::DetachedTimeout));
}
