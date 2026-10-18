// each test crate uses a different subset of these helpers.
#![allow(dead_code)]

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use quinn::Endpoint;
use ssj_net::{Identity, SERVER_NAME, client_tls, quic_client};
use ssj_proto::bootstrap::{Connect, format_pin, parse_connect};
use ssj_proto::{ClientFrame, ExitStatus, ServerFrame, decode_server};
use tokio::time::timeout;

/// Starts processes one at a time.
///
/// A test binary starts processes from several threads. On macOS a pipe is created and then marked
/// as not inheritable in a second step, and a process that another thread starts in between
/// inherits the pipe. The reading side of the test then waits for the end of a pipe that an
/// unrelated daemon keeps open. Taking a lock around the start avoids that.
pub trait SpawnLocked {
    fn spawn_locked(&mut self) -> std::io::Result<std::process::Child>;
}

impl SpawnLocked for std::process::Command {
    fn spawn_locked(&mut self) -> std::io::Result<std::process::Child> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        self.spawn()
    }
}

pub struct Launched {
    pub connect: Connect,
    pub stdout: Vec<u8>,
    pub closed_after: Duration,
}

/// A short directory for the sockets, since the path of a unix socket is limited to about 100 bytes.
pub fn runtime_dir() -> PathBuf {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let dir = PathBuf::from(format!(
        "/tmp/ssjt-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A UDP port that is free now. Fixed ports would collide with daemons of earlier runs.
pub fn free_range() -> (u16, u16) {
    let port = free_port();
    (port, port)
}

/// A UDP port for a server that is free now.
///
/// The ports come from a range below the ephemeral ports of macOS (from 49152) and of Linux (from
/// 32768). A port from the ephemeral range can be taken at any moment by the client socket of
/// another test, and then the packets for the server go to the wrong socket and a handshake waits
/// forever. Each test process starts at a different place of the range.
pub fn free_port() -> u16 {
    use std::net::UdpSocket;
    use std::sync::atomic::{AtomicU16, Ordering};

    static NEXT: AtomicU16 = AtomicU16::new(0);
    let base = 20_000 + (std::process::id() % 400) as u16 * 25;
    loop {
        let port = base + NEXT.fetch_add(1, Ordering::Relaxed) % 5_000;
        // free on both families, since the server listens on a dual stack socket
        let v4 = UdpSocket::bind(("0.0.0.0", port));
        let v6 = UdpSocket::bind(("::", port));
        if v4.is_ok() && v6.is_ok() {
            return port;
        }
    }
}

pub fn server_command(runtime: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ssj-server"));
    command.env("XDG_RUNTIME_DIR", runtime);
    command
}

pub fn run_bootstrap(
    runtime: &Path,
    stdin_text: &str,
    port_range: (u16, u16),
    script: &str,
) -> (std::process::ExitStatus, Vec<u8>, Vec<u8>, Duration) {
    run_bootstrap_with(runtime, stdin_text, port_range, script, &[])
}

/// Like [`run_bootstrap`] with more arguments for the server.
pub fn run_bootstrap_with(
    runtime: &Path,
    stdin_text: &str,
    port_range: (u16, u16),
    script: &str,
    extra: &[&str],
) -> (std::process::ExitStatus, Vec<u8>, Vec<u8>, Duration) {
    let mut child = server_command(runtime)
        .args(extra)
        .args([
            "--port-range",
            &if port_range == (0, 0) {
                "0".to_owned()
            } else {
                format!("{}:{}", port_range.0, port_range.1)
            },
        ])
        .args(["--detached-timeout", "20", "--shell", "/bin/sh"])
        .args(["--shell-arg", "-c", "--shell-arg", script])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn_locked()
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

pub fn launch(runtime: &Path, client: &Identity, port_range: (u16, u16), script: &str) -> Launched {
    launch_with(runtime, client, port_range, script, &[])
}

pub fn launch_with(
    runtime: &Path,
    client: &Identity,
    port_range: (u16, u16),
    script: &str,
    extra: &[&str],
) -> Launched {
    let pin = format_pin(&client.fingerprint());
    let (status, stdout, stderr, closed_after) =
        run_bootstrap_with(runtime, &pin, port_range, script, extra);
    assert!(status.success(), "{}", String::from_utf8_lossy(&stderr));
    let connect = parse_connect(&stdout).unwrap().expect("a CONNECT line");
    Launched {
        connect,
        stdout,
        closed_after,
    }
}

/// Runs `ssj-server` with control arguments and the given stdin, and waits for it.
pub fn run_control(runtime: &Path, args: &[&str], stdin_text: &str) -> Output {
    let mut child = server_command(runtime)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn_locked()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin_text.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

pub fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

pub fn endpoint(client: &Identity, server_fingerprint: ssj_net::Fingerprint) -> Endpoint {
    let mut endpoint = Endpoint::client("0.0.0.0:0".parse().unwrap()).unwrap();
    endpoint.set_default_client_config(quic_client(client_tls(client, server_fingerprint)));
    endpoint
}

pub async fn open(
    client: &Identity,
    connect: &Connect,
) -> Result<(quinn::SendStream, quinn::RecvStream, quinn::Connection), Box<dyn std::error::Error>> {
    let addr = ([127, 0, 0, 1], connect.udp.expect("a UDP port")).into();
    // a handshake that gets no answer must fail the test and not wait for ever
    let conn = timeout(
        Duration::from_secs(15),
        endpoint(client, connect.fingerprint).connect(addr, SERVER_NAME)?,
    )
    .await
    .map_err(|_| "the handshake got no answer within 15 seconds")??;
    let (tx, rx) = conn.open_bi().await?;
    Ok((tx, rx, conn))
}

pub async fn send(tx: &mut quinn::SendStream, frame: ClientFrame) {
    let mut bytes = Vec::new();
    frame.encode(&mut bytes).unwrap();
    tx.write_all(&bytes).await.unwrap();
}

/// Reads frames until `done` accepts the output so far, the exit arrives, or the stream ends.
pub async fn read_until(
    rx: &mut quinn::RecvStream,
    done: impl Fn(&str) -> bool,
) -> (String, Option<ExitStatus>) {
    let (mut buf, mut output, mut exit) = (Vec::new(), String::new(), None);
    timeout(Duration::from_secs(10), async {
        while !done(&output) && exit.is_none() {
            let mut chunk = [0u8; 4096];
            let Ok(Some(n)) = rx.read(&mut chunk).await else {
                break;
            };
            buf.extend_from_slice(&chunk[..n]);
            while let Some((frame, used)) = decode_server(&buf).unwrap() {
                buf.drain(..used);
                match frame {
                    ServerFrame::Output(data) => output.push_str(&String::from_utf8_lossy(&data)),
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

/// Whether the server accepts a client: it must answer, because a client finishes its side of
/// the handshake before the server has checked its certificate.
pub async fn reachable(client: &Identity, connect: &Connect) -> bool {
    let attempt = async {
        let (mut tx, mut rx, _conn) = open(client, connect).await.ok()?;
        let hello = ClientFrame::Hello(ssj_proto::Hello::Attach {
            size: ssj_proto::WindowSize::new(24, 80)?,
        });
        let mut bytes = Vec::new();
        hello.encode(&mut bytes).ok()?;
        tx.write_all(&bytes).await.ok()?;
        let mut chunk = [0u8; 64];
        rx.read(&mut chunk).await.ok()?.map(|_| ())
    };
    timeout(Duration::from_secs(3), attempt)
        .await
        .is_ok_and(|answer| answer.is_some())
}
