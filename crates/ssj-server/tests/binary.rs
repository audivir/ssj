use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use quinn::Endpoint;
use ssj_net::{Identity, SERVER_NAME, client_tls, quic_client};
use ssj_proto::bootstrap::{Connect, format_pin, parse_connect};
use ssj_proto::{ClientFrame, Env, ExitStatus, Hello, ServerFrame, WindowSize, decode_server};
use tokio::time::timeout;

/// A UDP port that is free now. Fixed ports would collide with daemons of earlier runs.
fn free_range() -> (u16, u16) {
    let port = free_port();
    (port, port)
}

fn free_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

struct Launched {
    connect: Connect,
    stdout: Vec<u8>,
    closed_after: Duration,
}

fn run_bootstrap(
    stdin_text: &str,
    port_range: (u16, u16),
    script: &str,
) -> (std::process::ExitStatus, Vec<u8>, Vec<u8>, Duration) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_ssj-server"))
        .args([
            "--port-range",
            &format!("{}:{}", port_range.0, port_range.1),
        ])
        .args(["--detached-timeout", "5", "--shell", "/bin/sh"])
        .args(["--shell-arg", "-c", "--shell-arg", script])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin_text.as_bytes())
        .unwrap();
    let start = Instant::now();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    child.stdout.take().unwrap().read_to_end(&mut out).unwrap();
    let closed_after = start.elapsed();
    child.stderr.take().unwrap().read_to_end(&mut err).unwrap();
    (child.wait().unwrap(), out, err, closed_after)
}

fn launch(client: &Identity, port_range: (u16, u16), script: &str) -> Launched {
    let pin = format_pin(&client.fingerprint());
    let (status, stdout, stderr, closed_after) = run_bootstrap(&pin, port_range, script);
    assert!(status.success(), "{}", String::from_utf8_lossy(&stderr));
    let connect = parse_connect(&stdout).unwrap().expect("a CONNECT line");
    Launched {
        connect,
        stdout,
        closed_after,
    }
}

fn endpoint(client: &Identity, server_fingerprint: ssj_net::Fingerprint) -> Endpoint {
    let mut endpoint = Endpoint::client("0.0.0.0:0".parse().unwrap()).unwrap();
    endpoint.set_default_client_config(quic_client(client_tls(client, server_fingerprint)));
    endpoint
}

async fn open(
    client: &Identity,
    launched: &Launched,
) -> Result<(quinn::SendStream, quinn::RecvStream, quinn::Connection), Box<dyn std::error::Error>> {
    let addr = ([127, 0, 0, 1], launched.connect.port).into();
    let conn = endpoint(client, launched.connect.fingerprint)
        .connect(addr, SERVER_NAME)?
        .await?;
    let (tx, rx) = conn.open_bi().await?;
    Ok((tx, rx, conn))
}

async fn send(tx: &mut quinn::SendStream, frame: ClientFrame) {
    let mut bytes = Vec::new();
    frame.encode(&mut bytes).unwrap();
    tx.write_all(&bytes).await.unwrap();
}

async fn read_exit(rx: &mut quinn::RecvStream) -> (String, Option<ExitStatus>) {
    let (mut buf, mut output, mut exit) = (Vec::new(), Vec::new(), None);
    timeout(Duration::from_secs(10), async {
        while exit.is_none() {
            let mut chunk = [0u8; 4096];
            let Ok(Some(n)) = rx.read(&mut chunk).await else {
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
    (String::from_utf8_lossy(&output).into_owned(), exit)
}

fn size() -> WindowSize {
    WindowSize::new(24, 80).unwrap()
}

/// Whether a new client can complete a connection and open a stream.
async fn reachable(client: &Identity, launched: &Launched) -> bool {
    timeout(Duration::from_secs(3), open(client, launched))
        .await
        .is_ok_and(|r| r.is_ok())
}

#[tokio::test]
async fn bootstrap_prints_one_line_releases_the_pipe_and_leaves_a_working_daemon() {
    let client = Identity::generate();
    let launched = launch(&client, free_range(), "read x; echo got:$x; exit 3");

    // exactly one line, so that nothing else can be mistaken for it
    assert_eq!(launched.stdout.iter().filter(|b| **b == b'\n').count(), 1);
    assert!(
        launched.closed_after < Duration::from_secs(5),
        "{:?}",
        launched.closed_after
    );

    // the parent is gone and its pipes are closed, yet the daemon serves the session
    let (mut tx, mut rx, _conn) = open(&client, &launched).await.unwrap();
    send(
        &mut tx,
        ClientFrame::Hello(Hello::Start {
            size: size(),
            env: Env::default(),
        }),
    )
    .await;
    send(&mut tx, ClientFrame::Input(b"binary\n".to_vec())).await;
    let (output, exit) = read_exit(&mut rx).await;

    assert!(output.contains("got:binary"), "{output}");
    assert_eq!(exit, Some(ExitStatus::Code(3)));

    // after the client confirms, the daemon ends and its port is closed
    send(&mut tx, ClientFrame::Close).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!reachable(&client, &launched).await);
}

#[tokio::test]
async fn detach_closes_the_endpoint_of_the_daemon() {
    let client = Identity::generate();
    let launched = launch(&client, free_range(), "cat");
    let (mut tx, mut rx, _conn) = open(&client, &launched).await.unwrap();
    send(
        &mut tx,
        ClientFrame::Hello(Hello::Start {
            size: size(),
            env: Env::default(),
        }),
    )
    .await;
    send(&mut tx, ClientFrame::Input(b"x\n".to_vec())).await;
    let mut chunk = [0u8; 64];
    timeout(Duration::from_secs(10), rx.read(&mut chunk))
        .await
        .unwrap()
        .unwrap();

    send(&mut tx, ClientFrame::Detach).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    assert!(!reachable(&client, &launched).await);
}

#[test]
fn bad_pin_line_is_refused_without_output() {
    for stdin in ["garbage\n", "", "SSJ PIN abc\n"] {
        let (status, stdout, stderr, _) = run_bootstrap(stdin, free_range(), "cat");
        assert_eq!(status.code(), Some(2), "{stdin:?}");
        assert!(stdout.is_empty(), "{stdin:?}");
        assert!(!stderr.is_empty(), "{stdin:?}");
    }
}

#[test]
fn single_port_range_is_used_and_a_busy_range_is_reported() {
    let client = Identity::generate();
    let range = free_range();
    let launched = launch(&client, range, "cat");
    assert_eq!(launched.connect.port, range.0);

    // the port is taken by the daemon now, so a second server finds nothing
    let pin = format_pin(&client.fingerprint());
    let (status, stdout, stderr, _) = run_bootstrap(&pin, range, "cat");
    assert_eq!(status.code(), Some(1));
    assert!(stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&stderr).contains("no free UDP port"),
        "{}",
        String::from_utf8_lossy(&stderr)
    );
}
