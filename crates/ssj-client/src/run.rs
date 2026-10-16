//! The session of the client: connect, pass bytes between terminal and server, and reconnect.

use std::net::SocketAddr;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use quinn::Endpoint;
use ssj_net::{Identity, SERVER_NAME, client_tls, quic_client};
use ssj_proto::bootstrap::Connect;
use ssj_proto::frame::MAX_DATA_LEN;
use ssj_proto::resume::check_welcome;
use ssj_proto::{
    ClientFrame, Env, ErrorCode, ExitStatus, Hello, ReplayBuffer, ServerFrame, WindowSize,
    decode_server,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{interval, sleep, timeout};

use crate::bootstrap::start_server;
use crate::cli::Args;
use crate::escape::{Action, EscapeFilter, HELP};
use crate::status::{LineStatus, Status, TitleStatus};
use crate::terminal::{RawMode, window_size};

/// Unacknowledged input the client keeps. Must stay below the input budget of the server.
const INPUT_BUFFER: usize = 1 << 20;
/// Time to deliver the last frames before the process ends.
const FLUSH_TIMEOUT: Duration = Duration::from_secs(2);
/// Time for one attempt to connect and to get the welcome.
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(8);
/// Pauses between attempts to reconnect, the last one repeats.
const BACKOFF: [Duration; 5] = [
    Duration::from_millis(250),
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(3),
];
const PING_EVERY: Duration = Duration::from_secs(2);
/// Silence of the server after which the client says so.
const STALL_AFTER: Duration = Duration::from_secs(5);

/// How the session ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Finished {
    /// The shell ended with this exit code.
    Exited(u8),
    /// The user left on purpose and the session keeps running.
    Detached,
}

/// Everything needed to talk to a started server.
pub struct Params {
    pub identity: Identity,
    pub connect: Connect,
    pub addr: SocketAddr,
    pub env: Env,
    /// `None` turns the escape character off.
    pub escape: Option<u8>,
}

/// The terminal side of a session, so that tests can stand in for it.
pub struct Io {
    pub input: Box<dyn AsyncRead + Send + Unpin>,
    pub output: Box<dyn AsyncWrite + Send + Unpin>,
    pub size: WindowSize,
    /// New window sizes.
    pub resizes: mpsc::UnboundedReceiver<WindowSize>,
    pub status: Box<dyn Status>,
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
    let prepared = async {
        let connect = start_server(&args, &identity.fingerprint()).await?;
        let env = local_env(&args)?;
        let addr = resolve(&args, connect.port).await?;
        Ok::<_, String>((connect, env, addr))
    }
    .await;
    let (connect, env, addr) = match prepared {
        Ok(prepared) => prepared,
        Err(message) => {
            eprintln!("ssj: {message}");
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
    let status: Box<dyn Status> = if raw.is_some() {
        Box::new(TitleStatus::default())
    } else {
        Box::new(LineStatus)
    };
    let (size_tx, resizes) = mpsc::unbounded_channel();
    if let Ok(mut winch) = signal(SignalKind::window_change()) {
        tokio::spawn(async move {
            while winch.recv().await.is_some() {
                if size_tx.send(window_size()).is_err() {
                    break;
                }
            }
        });
    }
    let io = Io {
        input: Box::new(tokio::io::stdin()),
        output: Box::new(tokio::io::stdout()),
        size: window_size(),
        resizes,
        status,
    };
    let result = run_session(
        Params {
            identity,
            connect,
            addr,
            env,
            escape: args.escape,
        },
        io,
    )
    .await;
    drop(raw);

    match result {
        Ok(Finished::Exited(code)) => code,
        Ok(Finished::Detached) => {
            eprintln!("[detached from session; it keeps running]");
            0
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

/// One connection to the server with its reader and writer.
struct Link {
    _conn: quinn::Connection,
    out: mpsc::UnboundedSender<ClientFrame>,
    writer: JoinHandle<()>,
    reader: FrameReader,
    last_heard: Instant,
}

impl Link {
    fn send(&self, frame: ClientFrame) {
        let _ = self.out.send(frame);
    }

    /// Lets the writer deliver what is queued, then ends the link.
    async fn finish(self) {
        drop(self.out);
        let _ = timeout(FLUSH_TIMEOUT * 2, self.writer).await;
    }
}

enum Attempt {
    Linked(Link),
    /// Try again later.
    Retry,
    /// The session cannot continue.
    Fatal(String),
}

enum Pumped {
    Done(Finished),
    Lost,
}

struct Client {
    endpoint: Endpoint,
    params: Params,
    io: Io,
    input: ReplayBuffer,
    output_offset: u64,
    escape: EscapeFilter,
    stdin_open: bool,
    chunk: Vec<u8>,
    size: WindowSize,
    /// Whether a `Start` may have reached the server already.
    started: bool,
}

pub async fn run_session(params: Params, io: Io) -> Result<Finished, String> {
    let bind: SocketAddr = if params.addr.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    }
    .parse()
    .expect("valid");
    let mut endpoint =
        Endpoint::client(bind).map_err(|e| format!("cannot open a UDP socket: {e}"))?;
    endpoint.set_default_client_config(quic_client(client_tls(
        &params.identity,
        params.connect.fingerprint,
    )));
    let size = io.size;
    let escape = EscapeFilter::new(params.escape);
    Client {
        endpoint,
        params,
        io,
        input: ReplayBuffer::new(INPUT_BUFFER),
        output_offset: 0,
        escape,
        stdin_open: true,
        chunk: vec![0u8; 16 * 1024],
        size,
        started: false,
    }
    .run()
    .await
}

impl Client {
    async fn run(mut self) -> Result<Finished, String> {
        let mut attempts = 0usize;
        loop {
            let hello = if self.started {
                Hello::Resume {
                    size: self.size,
                    output_offset: self.output_offset,
                }
            } else {
                Hello::Start {
                    size: self.size,
                    env: self.params.env.clone(),
                }
            };
            match self.establish(&hello).await {
                Attempt::Fatal(message) => {
                    self.clear_status().await;
                    return Err(message);
                }
                Attempt::Linked(mut link) => {
                    attempts = 0;
                    self.started = true;
                    self.clear_status().await;
                    match self.pump(&mut link).await? {
                        Pumped::Done(finished) => {
                            link.finish().await;
                            self.clear_status().await;
                            return Ok(finished);
                        }
                        Pumped::Lost => {}
                    }
                }
                Attempt::Retry => {}
            }
            attempts += 1;
            self.show_status(&format!(
                "connection lost, reconnecting (attempt {attempts})"
            ))
            .await;
            let pause = BACKOFF[attempts.saturating_sub(1).min(BACKOFF.len() - 1)];
            if let Some(finished) = self.wait_offline(pause).await? {
                self.clear_status().await;
                return Ok(finished);
            }
        }
    }

    async fn show_status(&mut self, text: &str) {
        let bytes = self.io.status.show(text);
        let _ = self.io.output.write_all(&bytes).await;
        let _ = self.io.output.flush().await;
    }

    async fn clear_status(&mut self) {
        let bytes = self.io.status.clear();
        let _ = self.io.output.write_all(&bytes).await;
        let _ = self.io.output.flush().await;
    }

    /// Connects, sends the hello, and resends the input that the server has not consumed.
    async fn establish(&mut self, hello: &Hello) -> Attempt {
        let attempt = timeout(ATTEMPT_TIMEOUT, async {
            let connecting = self.endpoint.connect(self.params.addr, SERVER_NAME).ok()?;
            let conn = connecting.await.ok()?;
            let (send, recv) = conn.open_bi().await.ok()?;
            let (out, out_rx) = mpsc::unbounded_channel();
            let writer = tokio::spawn(write_frames(send, out_rx));
            let mut link = Link {
                _conn: conn,
                out,
                writer,
                reader: FrameReader {
                    recv,
                    buf: Vec::new(),
                },
                last_heard: Instant::now(),
            };
            link.send(ClientFrame::Hello(hello.clone()));
            let reply = link.reader.next().await;
            Some((link, reply))
        })
        .await;
        let Ok(Some((link, reply))) = attempt else {
            return Attempt::Retry;
        };
        let welcome = match reply {
            Ok(Some(ServerFrame::Welcome(welcome))) => welcome,
            // the first start may not have arrived, so it is tried again as a start
            Ok(Some(ServerFrame::Error(ErrorCode::NotStarted)))
                if matches!(
                    hello,
                    Hello::Resume {
                        output_offset: 0,
                        ..
                    }
                ) && self.input.end() == 0 =>
            {
                self.started = false;
                return Attempt::Retry;
            }
            Ok(Some(ServerFrame::Error(ErrorCode::ReplayGap))) => {
                return Attempt::Fatal(
                    "the server no longer has the output since the connection was lost".to_owned(),
                );
            }
            Ok(Some(ServerFrame::Error(code))) => {
                return Attempt::Fatal(format!("the server refused the session: {code:?}"));
            }
            Err(message) => return Attempt::Fatal(message),
            Ok(_) => return Attempt::Retry,
        };
        if check_welcome(hello, welcome, self.input.start(), self.input.end()).is_err() {
            return Attempt::Fatal("the server answered with unexpected offsets".to_owned());
        }
        if self.input.ack(welcome.input_offset).is_err() {
            return Attempt::Fatal("the server consumed input that was never sent".to_owned());
        }
        // everything after the offset of the server is sent again, including what was typed offline
        let mut offset = welcome.input_offset;
        while let Ok(chunk) = self.input.read_from(offset, MAX_DATA_LEN) {
            if chunk.is_empty() {
                break;
            }
            offset += chunk.len() as u64;
            link.send(ClientFrame::Input(chunk));
        }
        Attempt::Linked(link)
    }

    /// Takes typed input into the buffer while there is no connection.
    ///
    /// Returns when the pause is over, or earlier if the user leaves with the escape.
    async fn wait_offline(&mut self, pause: Duration) -> Result<Option<Finished>, String> {
        let pause = sleep(pause);
        tokio::pin!(pause);
        loop {
            let room = self.input.free().saturating_sub(1).min(self.chunk.len());
            tokio::select! {
                _ = &mut pause => return Ok(None),
                read = self.io.input.read(&mut self.chunk[..room]), if self.stdin_open && room > 0 => {
                    match read {
                        Ok(n) if n > 0 => {
                            let typed = self.chunk[..n].to_vec();
                            if let Some(finished) = self.handle_typed(&typed, None).await? {
                                return Ok(Some(finished));
                            }
                        }
                        _ => self.stdin_open = false,
                    }
                }
                Some(size) = self.io.resizes.recv() => self.size = size,
            }
        }
    }

    /// Applies typed bytes: input for the shell, or a local escape.
    async fn handle_typed(
        &mut self,
        typed: &[u8],
        link: Option<&Link>,
    ) -> Result<Option<Finished>, String> {
        for action in self.escape.feed(typed) {
            match action {
                Action::Send(bytes) => {
                    let stored = self.input.push(&bytes);
                    if stored < bytes.len() {
                        // the reads leave room for the longest expansion, so this does not happen
                        return Err("the input buffer overflowed".to_owned());
                    }
                    if let Some(link) = link {
                        link.send(ClientFrame::Input(bytes));
                    }
                }
                Action::Detach => {
                    if let Some(link) = link {
                        link.send(ClientFrame::Detach);
                    }
                    return Ok(Some(Finished::Detached));
                }
                Action::Help => {
                    let _ = self.io.output.write_all(HELP.as_bytes()).await;
                    let _ = self.io.output.flush().await;
                }
            }
        }
        Ok(None)
    }

    /// Passes bytes between the terminal and the server until the session ends or the link breaks.
    async fn pump(&mut self, link: &mut Link) -> Result<Pumped, String> {
        let mut ping = interval(PING_EVERY);
        let mut stalled = false;
        loop {
            // two bytes of reserve: the escape can turn one typed byte into two
            let room = self.input.free().saturating_sub(2).min(self.chunk.len());
            tokio::select! {
                read = self.io.input.read(&mut self.chunk[..room]), if self.stdin_open && room > 0 => {
                    match read {
                        Ok(n) if n > 0 => {
                            let typed = self.chunk[..n].to_vec();
                            if let Some(finished) = self.handle_typed(&typed, Some(link)).await? {
                                return Ok(Pumped::Done(finished));
                            }
                        }
                        // end of input: the shell keeps running, like with `ssh`
                        _ => self.stdin_open = false,
                    }
                }
                frame = link.reader.next() => {
                    let Some(frame) = frame? else { return Ok(Pumped::Lost) };
                    link.last_heard = Instant::now();
                    if std::mem::take(&mut stalled) {
                        self.clear_status().await;
                    }
                    match frame {
                        ServerFrame::Output(data) => {
                            if self.io.output.write_all(&data).await.and(self.io.output.flush().await).is_err() {
                                return Err("cannot write to the terminal".to_owned());
                            }
                            self.output_offset += data.len() as u64;
                            if !link.reader.frame_ready() {
                                link.send(ClientFrame::OutputAck(self.output_offset));
                            }
                        }
                        ServerFrame::InputAck(offset) => {
                            self.input
                                .ack(offset)
                                .map_err(|_| "the server acknowledged input that was never sent".to_owned())?;
                        }
                        ServerFrame::Pong => {}
                        ServerFrame::Exit(status) => {
                            link.send(ClientFrame::Close);
                            return Ok(Pumped::Done(Finished::Exited(exit_code(status))));
                        }
                        ServerFrame::Error(code) => return Err(format!("the server ended the session: {code:?}")),
                        ServerFrame::Welcome(_) => return Err("the server sent a second welcome".to_owned()),
                    }
                }
                Some(size) = self.io.resizes.recv() => {
                    self.size = size;
                    link.send(ClientFrame::Resize(size));
                }
                _ = ping.tick() => {
                    link.send(ClientFrame::Ping);
                    let silent = link.last_heard.elapsed();
                    if silent > STALL_AFTER {
                        stalled = true;
                        self.show_status(&format!("no response from the server for {}s", silent.as_secs())).await;
                    }
                }
            }
        }
    }
}
