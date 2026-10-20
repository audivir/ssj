//! The session over TLS on TCP and over the stdin and stdout of `--stdio`.

mod common;

use std::process::Stdio;
use std::time::Duration;

use common::*;
use ssj_net::{Identity, tls_connector};
use ssj_proto::bootstrap::SYNC_HEADER;
use ssj_proto::{ClientFrame, Env, ExitStatus, Hello, ServerFrame, WindowSize, decode_server};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

fn size() -> WindowSize {
    WindowSize::new(24, 80).unwrap()
}

/// Frames to and from a byte stream, whatever carries it.
struct Wire<R, W> {
    reader: R,
    writer: W,
    buf: Vec<u8>,
}

impl<R: AsyncRead + Unpin, W: AsyncWrite + Unpin> Wire<R, W> {
    fn new(reader: R, writer: W) -> Self {
        Self {
            reader,
            writer,
            buf: Vec::new(),
        }
    }

    async fn send(&mut self, frame: ClientFrame) {
        let mut bytes = Vec::new();
        frame.encode(&mut bytes).unwrap();
        self.writer.write_all(&bytes).await.unwrap();
        self.writer.flush().await.unwrap();
    }

    /// Like [`Wire::send`], for a connection that the session may have closed: the process behind
    /// it can be gone already, and then the frame cannot be sent at all.
    async fn try_send(&mut self, frame: ClientFrame) {
        let mut bytes = Vec::new();
        frame.encode(&mut bytes).unwrap();
        if self.writer.write_all(&bytes).await.is_ok() {
            let _ = self.writer.flush().await;
        }
    }

    /// Next frame, or `None` at the end of the stream or on an error.
    async fn next(&mut self) -> Option<ServerFrame> {
        loop {
            if let Some((frame, used)) = decode_server(&self.buf).unwrap() {
                self.buf.drain(..used);
                return Some(frame);
            }
            let mut chunk = [0u8; 4096];
            match timeout(Duration::from_secs(10), self.reader.read(&mut chunk))
                .await
                .expect("timed out")
            {
                Ok(0) | Err(_) => return None,
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
            }
        }
    }

    /// Runs the session until the exit status arrives. Returns the output and the status.
    async fn run_to_the_end(&mut self, input: &[u8]) -> (String, Option<ExitStatus>) {
        self.send(ClientFrame::Hello(Hello::Start {
            size: size(),
            env: Env::default(),
            agent: false,
        }))
        .await;
        self.send(ClientFrame::Input(input.to_vec())).await;
        let mut output = String::new();
        let mut exit = None;
        while let Some(frame) = self.next().await {
            match frame {
                ServerFrame::Output(data) => output.push_str(&String::from_utf8_lossy(&data)),
                ServerFrame::Exit(status) => {
                    exit = Some(status);
                    break;
                }
                _ => {}
            }
        }
        (output, exit)
    }
}

async fn connect_tcp(
    client: &Identity,
    connect: &ssj_proto::bootstrap::Connect,
) -> std::io::Result<ssj_net::ClientStream<TcpStream>> {
    let tcp = TcpStream::connect(("127.0.0.1", connect.tcp.expect("a TCP port"))).await?;
    tls_connector(client, connect.fingerprint)
        .connect(ssj_net::server_name(), tcp)
        .await
}

const ASKING: &str = "read x; echo got:$x; exit 3";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_runs_over_tls_on_tcp() {
    let runtime = runtime_dir();
    let client = Identity::generate();
    let launched = launch(&runtime, &client, free_range(), ASKING);
    assert!(launched.connect.tcp.is_some() && launched.connect.udp.is_some());

    let stream = connect_tcp(&client, &launched.connect).await.unwrap();
    let (reader, writer) = tokio::io::split(stream);
    let (output, exit) = Wire::new(reader, writer).run_to_the_end(b"tcp\n").await;

    assert!(output.contains("got:tcp"), "{output:?}");
    assert_eq!(exit, Some(ExitStatus::Code(3)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tcp_refuses_a_client_with_another_certificate_and_the_right_one_still_works() {
    let runtime = runtime_dir();
    let client = Identity::generate();
    let launched = launch(&runtime, &client, free_range(), ASKING);

    let stranger = Identity::generate();
    // TLS 1.3 lets the client finish before the server has checked its certificate, so the
    // refusal shows when the client reads.
    if let Ok(stream) = connect_tcp(&stranger, &launched.connect).await {
        let (reader, writer) = tokio::io::split(stream);
        let mut wire = Wire::new(reader, writer);
        wire.send(ClientFrame::Hello(Hello::Start {
            size: size(),
            env: Env::default(),
            agent: false,
        }))
        .await;
        assert!(wire.next().await.is_none(), "a stranger got an answer");
    }

    let stream = connect_tcp(&client, &launched.connect).await.unwrap();
    let (reader, writer) = tokio::io::split(stream);
    let (_, exit) = Wire::new(reader, writer).run_to_the_end(b"x\n").await;
    assert_eq!(exit, Some(ExitStatus::Code(3)));
}

#[test]
fn each_transport_can_be_switched_off() {
    for (extra, udp, tcp) in [
        (&["--no-udp"][..], false, true),
        (&["--no-tcp"][..], true, false),
        (&["--no-udp", "--no-tcp"][..], false, false),
    ] {
        let runtime = runtime_dir();
        let launched = launch_with(&runtime, &Identity::generate(), free_range(), "cat", extra);
        assert_eq!(
            (
                launched.connect.udp.is_some(),
                launched.connect.tcp.is_some()
            ),
            (udp, tcp),
            "{extra:?}"
        );
    }
}

/// Starts `ssj-server --stdio` and reads up to the sync header.
async fn start_stdio(
    runtime: &std::path::Path,
    extra: &[&str],
) -> (
    tokio::process::Child,
    Wire<tokio::process::ChildStdout, tokio::process::ChildStdin>,
) {
    let mut child = tokio::process::Command::from({
        let mut command = server_command(runtime);
        command.arg("--stdio").args(extra);
        command
    })
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true)
    .spawn()
    .unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let stdin = child.stdin.take().unwrap();
    let mut seen = vec![0u8; SYNC_HEADER.len()];
    timeout(Duration::from_secs(10), stdout.read_exact(&mut seen))
        .await
        .expect("no sync header")
        .unwrap();
    assert_eq!(seen, SYNC_HEADER);
    (child, Wire::new(stdout, stdin))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_runs_over_stdin_and_stdout() {
    let runtime = runtime_dir();
    // no network transport at all: only the channel of `ssh` reaches this server.
    let launched = launch_with(
        &runtime,
        &Identity::generate(),
        free_range(),
        ASKING,
        &["--no-udp", "--no-tcp"],
    );
    assert_eq!((launched.connect.udp, launched.connect.tcp), (None, None));

    let id = launched.connect.id.as_str().to_owned();
    let (mut child, mut wire) = start_stdio(&runtime, &[&id]).await;
    let (output, exit) = wire.run_to_the_end(b"stdio\n").await;

    assert!(output.contains("got:stdio"), "{output:?}");
    assert_eq!(exit, Some(ExitStatus::Code(3)));
    // the client says it has the status, as the real one does.
    wire.send(ClientFrame::Close).await;
    // the process ends by itself once the session has ended.
    let status = timeout(Duration::from_secs(10), child.wait())
        .await
        .expect("the stdio process did not end")
        .unwrap();
    assert!(status.success());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stdio_without_a_running_session_says_so() {
    let runtime = runtime_dir();
    let output = run_control(&runtime, &["--stdio"], "");
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("no running session"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_stdio_connection_takes_over_and_the_session_goes_on() {
    let runtime = runtime_dir();
    let launched = launch(
        &runtime,
        &Identity::generate(),
        free_range(),
        "stty -echo; n=0; while read l; do n=$((n+1)); echo \"got:$l:$n\"; done",
    );
    let id = launched.connect.id.as_str().to_owned();

    let (_first_process, mut first) = start_stdio(&runtime, &[&id]).await;
    first
        .send(ClientFrame::Hello(Hello::Start {
            size: size(),
            env: Env::default(),
            agent: false,
        }))
        .await;
    first.send(ClientFrame::Input(b"one\n".to_vec())).await;
    let mut seen = String::new();
    while !seen.contains("got:one:1") {
        if let ServerFrame::Output(data) = first.next().await.expect("the stream ended") {
            seen.push_str(&String::from_utf8_lossy(&data));
        }
    }

    // the first process is still alive, as if its `ssh` had hung; the second one replaces it.
    let (_second_process, mut second) = start_stdio(&runtime, &[&id]).await;
    second
        .send(ClientFrame::Hello(Hello::Resume {
            size: size(),
            output_offset: seen.len() as u64,
        }))
        .await;
    second.send(ClientFrame::Input(b"two\n".to_vec())).await;
    let mut seen = String::new();
    while !seen.contains("got:two:2") {
        if let ServerFrame::Output(data) = second.next().await.expect("the stream ended") {
            seen.push_str(&String::from_utf8_lossy(&data));
        }
    }
}

// The first process is not dead but slow: what it sends after the second one has taken over must
// not reach the shell, and the second connection goes on without a gap.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn input_that_a_replaced_stdio_connection_sends_late_is_ignored() {
    let runtime = runtime_dir();
    let launched = launch(
        &runtime,
        &Identity::generate(),
        free_range(),
        "stty -echo; n=0; while read l; do n=$((n+1)); echo \"got:$l:$n\"; done",
    );
    let id = launched.connect.id.as_str().to_owned();

    let (_first_process, mut first) = start_stdio(&runtime, &[&id]).await;
    first
        .send(ClientFrame::Hello(Hello::Start {
            size: size(),
            env: Env::default(),
            agent: false,
        }))
        .await;
    first.send(ClientFrame::Input(b"one\n".to_vec())).await;
    let mut seen = String::new();
    while !seen.contains("got:one:1") {
        if let ServerFrame::Output(data) = first.next().await.expect("the stream ended") {
            seen.push_str(&String::from_utf8_lossy(&data));
        }
    }

    let (_second_process, mut second) = start_stdio(&runtime, &[&id]).await;
    second
        .send(ClientFrame::Hello(Hello::Resume {
            size: size(),
            output_offset: seen.len() as u64,
        }))
        .await;
    second.send(ClientFrame::Input(b"two\n".to_vec())).await;
    let mut after = String::new();
    while !after.contains("got:two:2") {
        if let ServerFrame::Output(data) = second.next().await.expect("the stream ended") {
            after.push_str(&String::from_utf8_lossy(&data));
        }
    }

    // the old connection wakes up and sends what it had held back.
    first.try_send(ClientFrame::Input(b"late\n".to_vec())).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    second.send(ClientFrame::Input(b"three\n".to_vec())).await;
    while !after.contains("got:three") {
        if let ServerFrame::Output(data) = second.next().await.expect("the stream ended") {
            after.push_str(&String::from_utf8_lossy(&data));
        }
    }
    assert!(!after.contains("late"), "{after:?}");
    assert!(after.contains("got:three:3"), "{after:?}");
}
