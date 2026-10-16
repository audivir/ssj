//! The client against an in-process server whose connections the test can kill.

use std::ffi::OsString;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use quinn::Endpoint;
use ssj_client::run::{Finished, Io, Params, run_session};
use ssj_client::status::Status;
use ssj_net::{Identity, quic_server, server_tls};
use ssj_proto::bootstrap::Connect;
use ssj_proto::{Env, EnvFilter, SessionId, WindowSize};
use ssj_server::backend::Direct;
use ssj_server::session::{Session, SessionConfig, SessionEnd, SessionEvent, SpawnTemplate};
use ssj_server::transport::serve_observed;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream, WriteHalf};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};

struct Server {
    addr: SocketAddr,
    fingerprint: ssj_net::Fingerprint,
    endpoint: Endpoint,
    config: quinn::ServerConfig,
    connections: mpsc::UnboundedReceiver<quinn::Connection>,
    events: mpsc::UnboundedReceiver<SessionEvent>,
    session: JoinHandle<SessionEnd>,
}

fn start_server(script: &str, client_pin: ssj_net::Fingerprint) -> Server {
    let identity = Identity::generate();
    let config = quic_server(server_tls(&identity, client_pin));
    let endpoint = Endpoint::server(config.clone(), "127.0.0.1:0".parse().unwrap()).unwrap();
    let addr = endpoint.local_addr().unwrap();
    let session_config = SessionConfig {
        id: SessionId::parse("abcdef012345").unwrap(),
        template: SpawnTemplate {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
            env: vec![(OsString::from("PATH"), OsString::from("/usr/bin:/bin"))],
            cwd: None,
        },
        accept_env: EnvFilter::default_patterns(),
        replay_capacity: 256 * 1024,
        detached_timeout: None,
        hello_timeout: Duration::from_secs(5),
    };
    let (conns, conns_rx) = mpsc::channel(4);
    let (observer, connections) = mpsc::unbounded_channel();
    let (events_tx, events) = mpsc::unbounded_channel();
    tokio::spawn(serve_observed(
        endpoint.clone(),
        conns,
        Duration::from_secs(5),
        Some(observer),
    ));
    let session = tokio::spawn(
        Session::new(Direct, session_config)
            .with_events(events_tx)
            .run(conns_rx),
    );
    Server {
        addr,
        fingerprint: identity.fingerprint(),
        endpoint,
        config,
        connections,
        events,
        session,
    }
}

/// UDP forwarder that can swallow all packets from the server to the client.
struct Proxy {
    addr: SocketAddr,
    drop_replies: Arc<AtomicBool>,
}

async fn start_proxy(server: SocketAddr) -> Proxy {
    let front = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let back = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    back.connect(server).await.unwrap();
    let addr = front.local_addr().unwrap();
    let drop_replies = Arc::new(AtomicBool::new(false));
    let dropping = drop_replies.clone();
    tokio::spawn(async move {
        let (mut from_client, mut from_server) = (vec![0u8; 65536], vec![0u8; 65536]);
        let mut client = None;
        loop {
            tokio::select! {
                Ok((n, peer)) = front.recv_from(&mut from_client) => {
                    client = Some(peer);
                    let _ = back.send(&from_client[..n]).await;
                }
                Ok(n) = back.recv(&mut from_server) => {
                    if let (Some(peer), false) = (client, dropping.load(Ordering::Relaxed)) {
                        let _ = front.send_to(&from_server[..n], peer).await;
                    }
                }
            }
        }
    });
    Proxy { addr, drop_replies }
}

/// Records the messages of the client.
struct Recorded(Arc<Mutex<Vec<String>>>);

impl Status for Recorded {
    fn show(&mut self, text: &str) -> Vec<u8> {
        self.0.lock().unwrap().push(text.to_owned());
        Vec::new()
    }

    fn clear(&mut self) -> Vec<u8> {
        self.0.lock().unwrap().push("<clear>".to_owned());
        Vec::new()
    }
}

struct Terminal {
    typed: WriteHalf<DuplexStream>,
    shown: tokio::io::ReadHalf<DuplexStream>,
    status: Arc<Mutex<Vec<String>>>,
    client: JoinHandle<Result<Finished, String>>,
    seen: String,
}

fn start_client(server: &Server, identity: Identity) -> Terminal {
    start_client_at(server, identity, server.addr)
}

fn start_client_at(server: &Server, identity: Identity, addr: SocketAddr) -> Terminal {
    start_client_with(server, identity, addr, false)
}

fn start_client_with(
    server: &Server,
    identity: Identity,
    addr: SocketAddr,
    attach: bool,
) -> Terminal {
    let (typed_tx, typed_rx) = tokio::io::duplex(1 << 20);
    let (shown_tx, shown_rx) = tokio::io::duplex(1 << 20);
    let (_, typed) = tokio::io::split(typed_tx);
    let (typed_rx, _) = tokio::io::split(typed_rx);
    let (_, shown_tx) = tokio::io::split(shown_tx);
    let (shown, _) = tokio::io::split(shown_rx);
    let (_resizes_tx, resizes) = mpsc::unbounded_channel();
    // keep the sender alive for the whole run
    std::mem::forget(_resizes_tx);
    let status = Arc::new(Mutex::new(Vec::new()));
    let io = Io {
        input: Box::new(typed_rx),
        output: Box::new(shown_tx),
        size: WindowSize::new(24, 80).unwrap(),
        resizes,
        status: Box::new(Recorded(status.clone())),
    };
    let params = Params {
        identity,
        connect: Connect {
            port: addr.port(),
            fingerprint: server.fingerprint,
        },
        addr,
        env: Env::default(),
        escape: Some(b'~'),
        attach,
    };
    let client = tokio::spawn(run_session(params, io));
    Terminal {
        typed,
        shown,
        status,
        client,
        seen: String::new(),
    }
}

impl Terminal {
    async fn type_line(&mut self, line: &str) {
        self.typed
            .write_all(format!("{line}\n").as_bytes())
            .await
            .unwrap();
    }

    async fn wait_for(&mut self, needle: &str) {
        let deadline = Duration::from_secs(30);
        timeout(deadline, async {
            while !self.seen.contains(needle) {
                let mut chunk = [0u8; 4096];
                let n = self.shown.read(&mut chunk).await.unwrap();
                assert!(n > 0, "the client closed the terminal");
                self.seen.push_str(&String::from_utf8_lossy(&chunk[..n]));
            }
        })
        .await
        .unwrap_or_else(|_| panic!("never saw {needle:?} in {:?}", self.seen));
    }

    /// Reads until nothing new has arrived for a while, so that repeated lines would show up.
    async fn settle(&mut self) {
        loop {
            let mut chunk = [0u8; 4096];
            match timeout(Duration::from_millis(600), self.shown.read(&mut chunk)).await {
                Ok(Ok(n)) if n > 0 => self.seen.push_str(&String::from_utf8_lossy(&chunk[..n])),
                _ => break,
            }
        }
        // a client that gave up would also make the output stop
        assert!(
            !self.client.is_finished(),
            "the client ended early: {:?}",
            self.status()
        );
    }

    /// The `got:` lines the shell printed, in order.
    fn got_lines(&self) -> Vec<String> {
        self.seen
            .lines()
            .map(str::trim)
            .filter(|l| l.starts_with("got:"))
            .map(str::to_owned)
            .collect()
    }

    fn status(&self) -> Vec<String> {
        self.status.lock().unwrap().clone()
    }
}

/// The newest connection that is still open, waiting for the client to make one if needed.
async fn live_connection(server: &mut Server) -> quinn::Connection {
    timeout(Duration::from_secs(15), async {
        loop {
            let conn = server.connections.recv().await.expect("the server ended");
            if conn.close_reason().is_none() {
                return conn;
            }
        }
    })
    .await
    .expect("no live connection to kill")
}

const ECHO_LINES: &str = "stty -echo; while read l; do echo \"got:$l\"; done";

#[tokio::test]
async fn killed_connections_lose_and_repeat_no_line() {
    let client_id = Identity::generate();
    let mut server = start_server(ECHO_LINES, client_id.fingerprint());
    let mut terminal = start_client(&server, client_id);

    for i in 0..60 {
        terminal.type_line(&format!("l{i}")).await;
        if i == 15 || i == 40 {
            sleep(Duration::from_millis(100)).await;
            live_connection(&mut server)
                .await
                .close(9u8.into(), b"killed by the test");
        }
        sleep(Duration::from_millis(5)).await;
    }
    terminal.wait_for("got:l59").await;
    terminal.settle().await;

    let expected: Vec<String> = (0..60).map(|i| format!("got:l{i}")).collect();
    assert_eq!(terminal.got_lines(), expected);
    let status = terminal.status();
    assert!(
        status.iter().any(|s| s.contains("reconnecting")),
        "{status:?}"
    );
    assert_eq!(
        status.last().map(String::as_str),
        Some("<clear>"),
        "{status:?}"
    );
}

#[tokio::test]
async fn input_typed_while_the_server_is_unreachable_arrives_once_it_returns() {
    let client_id = Identity::generate();
    let mut server = start_server(ECHO_LINES, client_id.fingerprint());
    let mut terminal = start_client(&server, client_id);

    terminal.type_line("before").await;
    terminal.wait_for("got:before").await;

    // the server stops accepting connections and drops the current one
    server.endpoint.set_server_config(None);
    server
        .connections
        .recv()
        .await
        .unwrap()
        .close(9u8.into(), b"killed by the test");
    sleep(Duration::from_millis(200)).await;
    for i in 0..10 {
        terminal.type_line(&format!("off{i}")).await;
    }
    sleep(Duration::from_millis(1500)).await;
    assert!(
        !terminal.status().is_empty(),
        "the client did not notice the outage"
    );

    server
        .endpoint
        .set_server_config(Some(server.config.clone()));
    terminal.wait_for("got:off9").await;
    terminal.settle().await;

    let mut expected = vec!["got:before".to_owned()];
    expected.extend((0..10).map(|i| format!("got:off{i}")));
    assert_eq!(terminal.got_lines(), expected);
    let attempts: Vec<_> = terminal
        .status()
        .into_iter()
        .filter(|s| s.contains("attempt"))
        .collect();
    // a silent server gives no refusal, so an attempt lasts until its timeout
    assert!(!attempts.is_empty(), "{attempts:?}");
}

#[tokio::test]
async fn escape_dot_detaches_and_the_session_keeps_running() {
    let client_id = Identity::generate();
    let mut server = start_server(ECHO_LINES, client_id.fingerprint());
    let mut terminal = start_client(&server, client_id);
    terminal.type_line("hello").await;
    terminal.wait_for("got:hello").await;

    terminal.typed.write_all(b"~.").await.unwrap();

    let finished = timeout(Duration::from_secs(10), terminal.client)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(finished, Ok(Finished::Detached));
    // other events (the client connecting) come first
    let detached = timeout(Duration::from_secs(10), async {
        while let Some(event) = server.events.recv().await {
            if event == SessionEvent::Detached {
                return true;
            }
        }
        false
    })
    .await
    .unwrap();
    assert!(detached);
    assert!(
        timeout(Duration::from_millis(500), &mut server.session)
            .await
            .is_err(),
        "the session ended"
    );
}

#[tokio::test]
async fn escape_dot_works_without_a_connection_so_the_user_is_never_stuck() {
    let client_id = Identity::generate();
    let mut server = start_server(ECHO_LINES, client_id.fingerprint());
    let mut terminal = start_client(&server, client_id);
    terminal.type_line("hello").await;
    terminal.wait_for("got:hello").await;

    server.endpoint.set_server_config(None);
    server
        .connections
        .recv()
        .await
        .unwrap()
        .close(9u8.into(), b"killed by the test");
    sleep(Duration::from_millis(500)).await;
    terminal.typed.write_all(b"\n~.").await.unwrap();

    let finished = timeout(Duration::from_secs(10), terminal.client)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(finished, Ok(Finished::Detached));
    // there was no connection to tell, so the server is not told
    while let Ok(event) = server.events.try_recv() {
        assert_ne!(event, SessionEvent::Detached);
    }
}

// The server consumed input whose acknowledgement never reached the client. After the
// reconnect the client must not send it a second time, and must get the output it missed.
#[tokio::test]
async fn input_consumed_before_the_loss_is_not_sent_twice() {
    let client_id = Identity::generate();
    let mut server = start_server(ECHO_LINES, client_id.fingerprint());
    let proxy = start_proxy(server.addr).await;
    let mut terminal = start_client_at(&server, client_id, proxy.addr);

    terminal.type_line("a0").await;
    terminal.wait_for("got:a0").await;

    proxy.drop_replies.store(true, Ordering::Relaxed);
    for i in 0..10 {
        terminal.type_line(&format!("b{i}")).await;
    }
    // the server has consumed and answered them by now, but the client heard nothing
    sleep(Duration::from_millis(800)).await;
    assert!(!terminal.seen.contains("got:b0"));
    server
        .connections
        .recv()
        .await
        .unwrap()
        .close(9u8.into(), b"killed by the test");
    proxy.drop_replies.store(false, Ordering::Relaxed);

    terminal.wait_for("got:b9").await;
    terminal.settle().await;
    let mut expected = vec!["got:a0".to_owned()];
    expected.extend((0..10).map(|i| format!("got:b{i}")));
    assert_eq!(terminal.got_lines(), expected);
    let status = terminal.status();
    assert!(
        status.iter().any(|s| s.contains("reconnecting")),
        "no reconnect happened: {status:?}"
    );
}

#[tokio::test]
async fn silent_server_is_reported_and_the_report_goes_away_when_it_answers() {
    let client_id = Identity::generate();
    let server = start_server(ECHO_LINES, client_id.fingerprint());
    let proxy = start_proxy(server.addr).await;
    let mut terminal = start_client_at(&server, client_id, proxy.addr);
    terminal.type_line("one").await;
    terminal.wait_for("got:one").await;

    proxy.drop_replies.store(true, Ordering::Relaxed);
    sleep(Duration::from_secs(8)).await;
    let during = terminal.status();
    assert!(
        during.iter().any(|s| s.contains("no response")),
        "{during:?}"
    );

    proxy.drop_replies.store(false, Ordering::Relaxed);
    terminal.type_line("two").await;
    terminal.wait_for("got:two").await;
    let after = terminal.status();
    assert_eq!(
        after.last().map(String::as_str),
        Some("<clear>"),
        "{after:?}"
    );
    // the same connection came back, so there was no reconnect
    assert!(
        !after.iter().any(|s| s.contains("reconnecting")),
        "{after:?}"
    );
}

// A client that restarts has no offsets. It joins with `Attach`, takes over the offsets of the
// server, and the shell goes on where it was.
#[tokio::test]
async fn attaching_client_continues_a_running_session_and_survives_a_later_loss() {
    let client_id = Identity::generate();
    let mut server = start_server(ECHO_COUNTING, client_id.fingerprint());
    let mut first = start_client(&server, client_id_clone(&client_id));
    first.type_line("one").await;
    first.wait_for("got:one:1").await;
    first.typed.write_all(b"~.").await.unwrap();
    let finished = timeout(Duration::from_secs(10), first.client)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(finished, Ok(Finished::Detached));

    let mut second = start_client_with(&server, client_id, server.addr, true);
    second.type_line("two").await;
    second.wait_for("got:two:2").await;
    // the second client has the history of the first one only as far as the replay reaches
    assert!(second.seen.contains("got:two:2"));

    // and it reconnects like any client
    live_connection(&mut server)
        .await
        .close(9u8.into(), b"killed by the test");
    second.type_line("three").await;
    second.wait_for("got:three:3").await;
    second.settle().await;
    assert!(second.status().iter().any(|s| s.contains("reconnecting")));
    let lines = second.got_lines();
    assert_eq!(
        lines.iter().filter(|l| l.starts_with("got:three")).count(),
        1
    );
}

const ECHO_COUNTING: &str =
    "stty -echo; n=0; while read l; do n=$((n+1)); echo \"got:$l:$n\"; done";

/// A second handle to the same identity, since the first one moves into a client.
fn client_id_clone(identity: &Identity) -> Identity {
    Identity {
        cert: identity.cert.clone(),
        key: identity.key.clone_key(),
    }
}
