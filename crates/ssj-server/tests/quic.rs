use std::ffi::OsString;
use std::net::SocketAddr;
use std::time::Duration;

use quinn::Endpoint;
use ssj_net::{Identity, SERVER_NAME, client_tls, quic_client, quic_server, server_tls};
use ssj_proto::{
    ClientFrame, Env, EnvFilter, ExitStatus, Hello, ServerFrame, SessionId, WindowSize,
    decode_server,
};
use ssj_server::backend::Direct;
use ssj_server::session::{Session, SessionConfig, SessionEnd, SpawnTemplate};
use ssj_server::transport::serve;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::timeout;

struct Server {
    addr: SocketAddr,
    server_id: Identity,
    session: JoinHandle<SessionEnd>,
    endpoint: Endpoint,
}

fn start(script: &str, client_pin: ssj_net::Fingerprint) -> Server {
    let server_id = Identity::generate();
    let endpoint = Endpoint::server(
        quic_server(server_tls(&server_id, client_pin)),
        "127.0.0.1:0".parse().unwrap(),
    )
    .unwrap();
    let addr = endpoint.local_addr().unwrap();
    let config = SessionConfig {
        id: SessionId::parse("abcdef012345").unwrap(),
        template: SpawnTemplate {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
            env: vec![(OsString::from("PATH"), OsString::from("/usr/bin:/bin"))],
            cwd: None,
        },
        accept_env: EnvFilter::default_patterns(),
        replay_capacity: 64 * 1024,
        away_after: Duration::from_secs(30),
        detached_timeout: None,
        max_detached: None,
        hello_timeout: Duration::from_secs(2),
    };
    let (conns, rx) = mpsc::channel(4);
    tokio::spawn(serve(endpoint.clone(), conns, Duration::from_secs(2)));
    let session = tokio::spawn(Session::new(Direct, config).run(rx));
    Server {
        addr,
        server_id,
        session,
        endpoint,
    }
}

fn client(identity: &Identity, server_pin: ssj_net::Fingerprint) -> Endpoint {
    let mut endpoint = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    endpoint.set_default_client_config(quic_client(client_tls(identity, server_pin)));
    endpoint
}

async fn send(stream: &mut quinn::SendStream, frame: ClientFrame) {
    let mut bytes = Vec::new();
    frame.encode(&mut bytes).unwrap();
    stream.write_all(&bytes).await.unwrap();
}

/// Reads frames until `done` accepts the output collected so far or the exit arrives.
async fn read_until(
    recv: &mut quinn::RecvStream,
    done: impl Fn(&[u8]) -> bool,
) -> (Vec<u8>, Option<ExitStatus>) {
    let (mut buf, mut output, mut exit) = (Vec::new(), Vec::new(), None);
    timeout(Duration::from_secs(10), async {
        while !done(&output) && exit.is_none() {
            let mut chunk = [0u8; 8192];
            let Ok(Some(n)) = recv.read(&mut chunk).await else {
                break;
            };
            buf.extend_from_slice(&chunk[..n]);
            while let Some((frame, used)) = decode_server(&buf).unwrap() {
                buf.drain(..used);
                match frame {
                    ServerFrame::Output(data) => output.extend_from_slice(&data),
                    ServerFrame::Exit(status) => exit = Some(status),
                    _ => {}
                }
            }
        }
    })
    .await
    .expect("timed out");
    (output, exit)
}

fn size() -> WindowSize {
    WindowSize::new(24, 80).unwrap()
}

#[tokio::test]
async fn pinned_client_runs_a_shell_over_quic_and_gets_the_exit_code() {
    let client_id = Identity::generate();
    let server = start("read x; echo got:$x; exit 7", client_id.fingerprint());
    let endpoint = client(&client_id, server.server_id.fingerprint());
    let conn = endpoint
        .connect(server.addr, SERVER_NAME)
        .unwrap()
        .await
        .unwrap();
    let (mut tx, mut rx) = conn.open_bi().await.unwrap();

    send(
        &mut tx,
        ClientFrame::Hello(Hello::Start {
            size: size(),
            env: Env::default(),
        }),
    )
    .await;
    send(&mut tx, ClientFrame::Input(b"quic\n".to_vec())).await;
    let (output, exit) = read_until(&mut rx, |_| false).await;

    assert!(String::from_utf8_lossy(&output).contains("got:quic"));
    assert_eq!(exit, Some(ExitStatus::Code(7)));
    send(&mut tx, ClientFrame::Close).await;
    let end = timeout(Duration::from_secs(10), server.session)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(end, SessionEnd::ShellExited(status) if status.code() == Some(7)));
}

// The handshake fails before a stream exists, so the session never learns about the attempt.
#[tokio::test]
async fn client_with_another_certificate_never_reaches_the_session() {
    let (pinned, stranger) = (Identity::generate(), Identity::generate());
    let server = start("cat", pinned.fingerprint());
    let endpoint = client(&stranger, server.server_id.fingerprint());

    let attempt = async {
        let conn = endpoint.connect(server.addr, SERVER_NAME).unwrap().await?;
        // the client may finish its side of the handshake first, so it has to use the stream
        let (mut tx, mut rx) = conn.open_bi().await?;
        let mut bytes = Vec::new();
        ClientFrame::Hello(Hello::Attach { size: size() })
            .encode(&mut bytes)
            .unwrap();
        tx.write_all(&bytes).await?;
        let mut chunk = [0u8; 64];
        let n = rx.read(&mut chunk).await?;
        Ok::<_, Box<dyn std::error::Error>>(n)
    };
    assert!(!matches!(attempt.await, Ok(Some(_))));

    // nothing arrived at the session, which is still waiting for its first connection
    assert!(
        timeout(Duration::from_millis(500), server.session)
            .await
            .is_err()
    );
    drop(server.endpoint);
}

#[tokio::test]
async fn newer_quic_connection_replaces_the_older_one() {
    let client_id = Identity::generate();
    let server = start("cat", client_id.fingerprint());

    let first_endpoint = client(&client_id, server.server_id.fingerprint());
    let first = first_endpoint
        .connect(server.addr, SERVER_NAME)
        .unwrap()
        .await
        .unwrap();
    let (mut tx, mut rx) = first.open_bi().await.unwrap();
    send(
        &mut tx,
        ClientFrame::Hello(Hello::Start {
            size: size(),
            env: Env::default(),
        }),
    )
    .await;
    send(&mut tx, ClientFrame::Input(b"first\n".to_vec())).await;
    read_until(&mut rx, |o| String::from_utf8_lossy(o).contains("first")).await;

    let second_endpoint = client(&client_id, server.server_id.fingerprint());
    let second = second_endpoint
        .connect(server.addr, SERVER_NAME)
        .unwrap()
        .await
        .unwrap();
    let (mut tx2, mut rx2) = second.open_bi().await.unwrap();
    send(&mut tx2, ClientFrame::Hello(Hello::Attach { size: size() })).await;
    send(&mut tx2, ClientFrame::Input(b"second\n".to_vec())).await;
    let (output, _) = read_until(&mut rx2, |o| String::from_utf8_lossy(o).contains("second")).await;

    assert!(String::from_utf8_lossy(&output).contains("second"));
    timeout(Duration::from_secs(10), first.closed())
        .await
        .expect("the old connection is closed");
}
