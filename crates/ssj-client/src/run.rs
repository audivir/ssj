//! The session of the client: connect, pass bytes between terminal and server, and exit.

use std::net::SocketAddr;
use std::process::ExitCode;
use std::time::Duration;

use quinn::Endpoint;
use ssj_net::{Identity, SERVER_NAME, client_tls, quic_client};
use ssj_proto::resume::check_welcome;
use ssj_proto::{ClientFrame, Env, ExitStatus, Hello, ReplayBuffer, ServerFrame, decode_server};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;
use tokio::time::timeout;

use crate::bootstrap::start_server;
use crate::cli::Args;
use crate::terminal::{RawMode, window_size};

/// Unacknowledged input the client keeps. Must stay below the input budget of the server.
const INPUT_BUFFER: usize = 1 << 20;
/// Time to deliver the last frames before the process ends.
const FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

enum Outcome {
    /// The shell ended with this exit code.
    Exit(u8),
    /// The connection ended before the shell did.
    Lost,
}

pub fn run(args: Args) -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("ssj: cannot start the runtime: {err}");
            return ExitCode::from(1);
        }
    };
    let code = runtime.block_on(session(args));
    // the read of stdin runs on a worker thread that never returns on a terminal, so waiting for
    // the runtime to finish would hang forever
    runtime.shutdown_background();
    ExitCode::from(code)
}

/// Selected variables of the local environment.
fn local_env(args: &Args) -> Result<Env, String> {
    let vars = std::env::vars_os()
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
        .filter(|(name, _)| args.send_env.matches(name))
        .collect();
    Env::new(vars).map_err(|e| format!("the selected environment is not valid: {e:?}"))
}

async fn resolve(args: &Args, port: u16) -> Result<SocketAddr, String> {
    let host = args.udp_host.as_deref().unwrap_or_else(|| args.host());
    let host = host.trim_start_matches('[').trim_end_matches(']');
    tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| format!("cannot resolve {host}: {e}"))?
        .next()
        .ok_or_else(|| format!("{host} has no address"))
}

async fn session(args: Args) -> u8 {
    let identity = Identity::generate();
    let connect = match start_server(&args, &identity.fingerprint()).await {
        Ok(connect) => connect,
        Err(message) => {
            eprintln!("ssj: {message}");
            return 1;
        }
    };
    let env = match local_env(&args) {
        Ok(env) => env,
        Err(message) => {
            eprintln!("ssj: {message}");
            return 1;
        }
    };
    let addr = match resolve(&args, connect.port).await {
        Ok(addr) => addr,
        Err(message) => {
            eprintln!("ssj: {message}");
            return 1;
        }
    };

    let bind: SocketAddr = if addr.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    }
    .parse()
    .expect("valid");
    let mut endpoint = match Endpoint::client(bind) {
        Ok(endpoint) => endpoint,
        Err(err) => {
            eprintln!("ssj: cannot open a UDP socket: {err}");
            return 1;
        }
    };
    endpoint.set_default_client_config(quic_client(client_tls(&identity, connect.fingerprint)));
    let connecting = match endpoint.connect(addr, SERVER_NAME) {
        Ok(connecting) => connecting,
        Err(err) => {
            eprintln!("ssj: {err}");
            return 1;
        }
    };
    let conn = match timeout(Duration::from_secs(15), connecting).await {
        Ok(Ok(conn)) => conn,
        Ok(Err(err)) => {
            eprintln!("ssj: cannot connect to {addr}: {err}");
            return 1;
        }
        Err(_) => {
            eprintln!("ssj: cannot connect to {addr}: timed out (is the UDP port reachable?)");
            return 1;
        }
    };

    let raw = match RawMode::enable() {
        Ok(raw) => raw,
        Err(err) => {
            eprintln!("ssj: cannot set up the terminal: {err}");
            return 1;
        }
    };
    let hello = Hello::Start {
        size: window_size(),
        env,
    };
    let result = interactive(&conn, hello).await;
    drop(raw);

    match result {
        Ok(Outcome::Exit(code)) => code,
        Ok(Outcome::Lost) => {
            eprintln!("ssj: connection lost");
            1
        }
        Err(message) => {
            eprintln!("ssj: {message}");
            1
        }
    }
}

/// Reads server frames from a stream, keeping bytes of incomplete frames between calls.
struct FrameReader {
    recv: quinn::RecvStream,
    buf: Vec<u8>,
}

impl FrameReader {
    /// Next frame, or `None` when the stream ended.
    async fn next(&mut self) -> Result<Option<ServerFrame>, String> {
        loop {
            match decode_server(&self.buf) {
                Ok(Some((frame, used))) => {
                    self.buf.drain(..used);
                    return Ok(Some(frame));
                }
                Ok(None) => {}
                Err(err) => return Err(format!("the server sent an invalid frame: {err:?}")),
            }
            let mut chunk = [0u8; 16 * 1024];
            match self.recv.read(&mut chunk).await {
                Ok(Some(n)) => self.buf.extend_from_slice(&chunk[..n]),
                Ok(None) | Err(_) => return Ok(None),
            }
        }
    }

    /// Whether a complete frame is waiting, so that acknowledgements can be sent per batch.
    fn frame_ready(&self) -> bool {
        matches!(decode_server(&self.buf), Ok(Some(_)))
    }
}

/// Sends frames from a channel, so that a stalled stream never blocks reading.
async fn write_frames(
    mut send: quinn::SendStream,
    mut frames: mpsc::UnboundedReceiver<ClientFrame>,
) {
    let mut bytes = Vec::new();
    while let Some(frame) = frames.recv().await {
        bytes.clear();
        if frame.encode(&mut bytes).is_err() || send.write_all(&bytes).await.is_err() {
            return;
        }
    }
    let _ = send.finish();
    // the peer must have the last frame before the process ends
    let _ = timeout(FLUSH_TIMEOUT, send.stopped()).await;
}

fn exit_code(status: ExitStatus) -> u8 {
    match status {
        ExitStatus::Code(code) => code,
        ExitStatus::Signal(signal) => 128u8.saturating_add(signal),
    }
}

async fn interactive(conn: &quinn::Connection, hello: Hello) -> Result<Outcome, String> {
    let (send, recv) = conn
        .open_bi()
        .await
        .map_err(|e| format!("cannot open the stream: {e}"))?;
    let (out, out_rx) = mpsc::unbounded_channel();
    let writer = tokio::spawn(write_frames(send, out_rx));
    let mut reader = FrameReader {
        recv,
        buf: Vec::new(),
    };
    let send_frame = |frame: ClientFrame| {
        let _ = out.send(frame);
    };

    send_frame(ClientFrame::Hello(hello.clone()));
    let welcome = match reader.next().await? {
        Some(ServerFrame::Welcome(welcome)) => welcome,
        Some(ServerFrame::Error(code)) => {
            return Err(format!("the server refused the session: {code:?}"));
        }
        _ => return Ok(Outcome::Lost),
    };
    check_welcome(&hello, welcome, 0, 0)
        .map_err(|_| "the server answered with unexpected offsets".to_owned())?;

    let mut input = ReplayBuffer::new(INPUT_BUFFER);
    let mut output_offset = welcome.output_offset;
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut stdin_open = true;
    let mut winch = signal(SignalKind::window_change())
        .map_err(|e| format!("cannot watch the window size: {e}"))?;
    let mut chunk = vec![0u8; 16 * 1024];

    let outcome = loop {
        let room = input.free().min(chunk.len());
        tokio::select! {
            read = stdin.read(&mut chunk[..room]), if stdin_open && room > 0 => match read {
                Ok(n) if n > 0 => {
                    input.push(&chunk[..n]);
                    send_frame(ClientFrame::Input(chunk[..n].to_vec()));
                }
                // end of input: the shell keeps running, like with `ssh`
                _ => stdin_open = false,
            },
            frame = reader.next() => match frame? {
                None => break Outcome::Lost,
                Some(ServerFrame::Output(data)) => {
                    if stdout.write_all(&data).await.and(stdout.flush().await).is_err() {
                        break Outcome::Lost;
                    }
                    output_offset += data.len() as u64;
                    if !reader.frame_ready() {
                        send_frame(ClientFrame::OutputAck(output_offset));
                    }
                }
                Some(ServerFrame::InputAck(offset)) => {
                    input.ack(offset).map_err(|_| "the server acknowledged input that was never sent".to_owned())?;
                }
                Some(ServerFrame::Exit(status)) => {
                    send_frame(ClientFrame::Close);
                    break Outcome::Exit(exit_code(status));
                }
                Some(ServerFrame::Error(code)) => return Err(format!("the server ended the session: {code:?}")),
                Some(ServerFrame::Welcome(_)) => return Err("the server sent a second welcome".to_owned()),
            },
            _ = winch.recv() => send_frame(ClientFrame::Resize(window_size())),
        }
    };
    drop(out);
    let _ = timeout(FLUSH_TIMEOUT * 2, writer).await;
    Ok(outcome)
}
