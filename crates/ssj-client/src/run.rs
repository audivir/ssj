//! The session of the client: connect, pass bytes between terminal and server, and reconnect.

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use quinn::Endpoint;
use ssj_net::{Identity, SERVER_NAME, client_tls, quic_client};
use ssj_proto::bootstrap::Connect;
use ssj_proto::control::{Reply, SessionInfo, SessionState, parse_reply};
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

use crate::bootstrap::{StartError, run_remote, start_server};
use crate::cli::{Args, Command};
use crate::config::Config;
use crate::debounce::Debounce;
use crate::escape::{Action, EscapeFilter, HELP};
use crate::install::{Decision, confirm, decide, install, interactive};
use crate::netwatch::Cause;
use crate::status::{LineStatus, Status, TitleStatus, dropped_notice};
use crate::terminal::{RawMode, window_size};
use crate::tracker::Tracker;

/// Unacknowledged input the client keeps. Must stay below the input budget of the server.
const INPUT_BUFFER: usize = 1 << 20;
/// Time to deliver the last frames before the process ends.
const FLUSH_TIMEOUT: Duration = Duration::from_secs(2);
/// Time for one attempt to connect and to get the welcome.
pub const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(8);
/// Pauses between attempts to reconnect, the last one repeats.
const BACKOFF: [Duration; 5] = [
    Duration::from_millis(250),
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(3),
];
/// A new lookup of the server address happens before every attempt after this many failed ones.
const RESOLVE_EVERY: usize = 3;
/// Time a lookup may take before the old address is kept.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(2);
/// A resize is sent when no newer one has come for this long.
const RESIZE_QUIET: Duration = Duration::from_millis(50);
const PING_EVERY: Duration = Duration::from_secs(1);
/// The client says that the server is silent after four round trips, but not sooner than this
/// and not later than the maximum, so that a good link reports a stall quickly and a slow link
/// does not report one all the time.
const STALL_MIN: Duration = Duration::from_secs(2);
const STALL_MAX: Duration = Duration::from_secs(10);
/// A change of network produces many notifications. One reaction follows when they stop.
const CHANGE_QUIET: Duration = Duration::from_millis(300);
/// Time the server gets to answer after the client moved to a new network path, before the client
/// starts a new connection. A wake from sleep leaves less hope for the old path.
const PROBE_AFTER_CHANGE: Duration = Duration::from_secs(2);
const PROBE_AFTER_WAKE: Duration = Duration::from_secs(1);

/// How the session ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Finished {
    /// The shell ended with this exit code.
    Exited(u8),
    /// The user left on purpose and the session keeps running.
    Detached,
}

/// Looks up the address of the server again.
pub type Resolver =
    Box<dyn FnMut() -> Pin<Box<dyn Future<Output = Result<SocketAddr, String>> + Send>> + Send>;

/// Everything needed to talk to a started server.
pub struct Params {
    pub identity: Identity,
    pub connect: Connect,
    pub addr: SocketAddr,
    /// Finds the address again when it may have changed. `None` keeps `addr` for good.
    pub resolver: Option<Resolver>,
    /// Time for one attempt to connect and to get the welcome.
    pub attempt_timeout: Duration,
    pub env: Env,
    /// `None` turns the escape character off.
    pub escape: Option<u8>,
    /// Join a running session instead of starting one.
    pub attach: bool,
}

/// The terminal side of a session, so that tests can stand in for it.
pub struct Io {
    pub input: Box<dyn AsyncRead + Send + Unpin>,
    pub output: Box<dyn AsyncWrite + Send + Unpin>,
    pub size: WindowSize,
    /// New window sizes.
    pub resizes: mpsc::UnboundedReceiver<WindowSize>,
    pub status: Box<dyn Status>,
    /// Changes of the network and wakes from sleep. A channel that never sends is fine.
    pub net: mpsc::UnboundedReceiver<Cause>,
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

async fn resolve(host: &str, port: u16) -> Result<SocketAddr, String> {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| format!("cannot resolve {host}: {e}"))?
        .next()
        .ok_or_else(|| format!("{host} has no address"))
}

async fn session(mut args: Args) -> u8 {
    let config = match Config::load() {
        Ok(config) => config,
        Err(message) => {
            eprintln!("ssj: {message}");
            return 2;
        }
    };
    args.apply_config(&config);
    match args.command.clone() {
        Command::Connect => connect_session(args, &config, &[], false).await,
        Command::Attach(id) => {
            let mut extra = vec!["--attach".to_owned()];
            extra.extend(id.map(|id| id.as_str().to_owned()));
            connect_session(args, &config, &extra, true).await
        }
        Command::List => list_sessions(&args).await,
        Command::Kill(id) => kill_session(&args, id).await,
        Command::Install => match install(&args).await {
            Ok(path) => {
                println!("installed {path}");
                0
            }
            Err(message) => {
                eprintln!("ssj: {message}");
                1
            }
        },
    }
}

/// Starts the server. If it is missing, installs it when the policy and the user agree.
async fn start_or_install(
    args: &Args,
    config: &Config,
    fingerprint: &[u8; 32],
    extra: &[String],
    attach: bool,
) -> Result<Connect, String> {
    match start_server(args, fingerprint, extra).await {
        Ok(connect) => Ok(connect),
        Err(StartError::Failed(message)) => Err(message),
        Err(StartError::Missing) if attach => Err(format!(
            "ssj-server is not installed on {}, so no session runs there",
            args.destination
        )),
        Err(StartError::Missing) => {
            let policy = args.auto_install.unwrap_or(config.auto_install);
            match decide(policy, &args.destination, interactive(), confirm) {
                Decision::Skip(message) => Err(message),
                Decision::Install => {
                    let path = install(args).await?;
                    eprintln!("ssj: installed {path}");
                    match start_server(args, fingerprint, extra).await {
                        Ok(connect) => Ok(connect),
                        Err(StartError::Missing) => {
                            Err("the server is still missing after the installation".to_owned())
                        }
                        Err(StartError::Failed(message)) => Err(message),
                    }
                }
            }
        }
    }
}

/// Text for the age of a session: `5s`, `3m`, `2h`, or `4d`.
pub fn format_age(seconds: u64) -> String {
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m", seconds / 60),
        3600..86400 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86400),
    }
}

fn state_word(state: SessionState) -> &'static str {
    match state {
        SessionState::Connected => "connected",
        SessionState::Disconnected => "disconnected",
        SessionState::Detached => "detached",
    }
}

async fn list_sessions(args: &Args) -> u8 {
    let remote = match run_remote(args, &["--list".to_owned()], "").await {
        Ok(remote) if remote.server_missing() => {
            println!(
                "no sessions (ssj-server is not installed on {})",
                args.destination
            );
            return 0;
        }
        Ok(remote) if remote.status.success() => remote,
        Ok(remote) => {
            eprintln!(
                "ssj: could not list the sessions ({}): {}",
                remote.status,
                remote.error_text()
            );
            return 1;
        }
        Err(message) => {
            eprintln!("ssj: {message}");
            return 1;
        }
    };
    let infos: Vec<SessionInfo> = remote
        .stdout
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .filter_map(|line| match parse_reply(line) {
            Ok(Reply::Info(info)) => Some(info),
            _ => None,
        })
        .collect();
    if infos.is_empty() {
        println!("no sessions");
    }
    for info in infos {
        println!(
            "{}  {:<12}  {:>4}  protocol {}",
            info.id.as_str(),
            state_word(info.state),
            format_age(info.age_secs),
            info.protocol
        );
    }
    0
}

async fn kill_session(args: &Args, id: Option<ssj_proto::SessionId>) -> u8 {
    let mut extra = vec!["--kill".to_owned()];
    extra.extend(id.map(|id| id.as_str().to_owned()));
    match run_remote(args, &extra, "").await {
        Ok(remote) if remote.server_missing() => {
            eprintln!(
                "ssj: no running session (ssj-server is not installed on {})",
                args.destination
            );
            1
        }
        Ok(remote) if remote.status.success() => {
            println!("session ended");
            0
        }
        Ok(remote) => {
            eprintln!("ssj: could not end the session: {}", remote.error_text());
            1
        }
        Err(message) => {
            eprintln!("ssj: {message}");
            1
        }
    }
}

async fn connect_session(args: Args, config: &Config, extra: &[String], attach: bool) -> u8 {
    let identity = Identity::generate();
    let prepared = async {
        let connect =
            start_or_install(&args, config, &identity.fingerprint(), extra, attach).await?;
        let env = local_env(&args)?;
        let host = args.udp_host.as_deref().unwrap_or_else(|| args.host());
        let addr = resolve(host, connect.port).await?;
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
        net: crate::netwatch::start().events,
    };
    let result = run_session(
        Params {
            identity,
            connect,
            addr,
            resolver: Some(Box::new({
                let host = args
                    .udp_host
                    .clone()
                    .unwrap_or_else(|| args.host().to_owned());
                let port = connect.port;
                move || {
                    let host = host.clone();
                    Box::pin(async move { resolve(&host, port).await })
                }
            })),
            attempt_timeout: ATTEMPT_TIMEOUT,
            env,
            escape: args.escape,
            attach,
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

/// The next cause from the watch, or never if there is none.
async fn next_cause(net: &mut Option<mpsc::UnboundedReceiver<Cause>>) -> Option<Cause> {
    match net {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

fn exit_code(status: ExitStatus) -> u8 {
    match status {
        ExitStatus::Code(code) => code,
        ExitStatus::Signal(signal) => 128u8.saturating_add(signal),
    }
}

/// One connection to the server with its reader and writer.
struct Link {
    conn: quinn::Connection,
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
    net: Option<mpsc::UnboundedReceiver<Cause>>,
    tracker: Tracker,
    /// The message that should be on the screen, and the one that is.
    status_wanted: Option<String>,
    status_shown: Option<String>,
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
    let mut io = io;
    let net = Some(std::mem::replace(&mut io.net, mpsc::unbounded_channel().1));
    Client {
        endpoint,
        params,
        io,
        net,
        tracker: Tracker::new(),
        status_wanted: None,
        status_shown: None,
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
            } else if self.params.attach {
                Hello::Attach { size: self.size }
            } else {
                Hello::Start {
                    size: self.size,
                    env: self.params.env.clone(),
                }
            };
            if attempts > 0 && attempts.is_multiple_of(RESOLVE_EVERY) {
                self.refresh_address().await;
            }
            match self.establish_watched(&hello).await {
                Attempt::Fatal(message) => {
                    self.clear_status_at_end().await;
                    return Err(message);
                }
                Attempt::Linked(mut link) => {
                    attempts = 0;
                    self.started = true;
                    self.clear_status().await;
                    match self.pump(&mut link).await? {
                        Pumped::Done(finished) => {
                            link.finish().await;
                            self.clear_status_at_end().await;
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
                self.clear_status_at_end().await;
                return Ok(finished);
            }
        }
    }

    async fn show_status(&mut self, text: &str) {
        self.status_wanted = Some(text.to_owned());
        self.sync_status(false).await;
    }

    async fn clear_status(&mut self) {
        self.status_wanted = None;
        self.sync_status(false).await;
    }

    /// Ends the display for good, even inside a sequence: nothing more will arrive to finish it.
    async fn clear_status_at_end(&mut self) {
        self.status_wanted = None;
        self.sync_status(true).await;
    }

    /// Writes the status change that is waiting, unless that would split a sequence of the remote
    /// side or a character. It is retried after every piece of output.
    async fn sync_status(&mut self, force: bool) {
        if self.status_wanted == self.status_shown || (!force && self.tracker.in_sequence()) {
            return;
        }
        let bytes = match &self.status_wanted {
            Some(text) => self.io.status.show(text),
            None => self.io.status.clear(self.tracker.title()),
        };
        let _ = self.io.output.write_all(&bytes).await;
        let _ = self.io.output.flush().await;
        self.status_shown = self.status_wanted.clone();
    }

    /// Writes the notice for `dropped` forgotten bytes into the output.
    async fn note_gap(&mut self, dropped: u64) {
        let _ = self.io.output.write_all(&dropped_notice(dropped)).await;
        let _ = self.io.output.flush().await;
        // the rest of an open sequence is gone, so the tracker must not wait for it
        self.tracker.abandon_sequence();
        self.sync_status(false).await;
    }

    /// Moves the UDP socket to the current network. QUIC then probes the new path at once, and
    /// the server follows the new address.
    fn rebind(&mut self) {
        let local: SocketAddr = if self.params.addr.is_ipv4() {
            "0.0.0.0:0"
        } else {
            "[::]:0"
        }
        .parse()
        .expect("valid");
        if let Ok(socket) = std::net::UdpSocket::bind(local) {
            let _ = self.endpoint.rebind(socket);
        }
    }

    /// Looks up the address of the server again, since a move or a long sleep may have changed
    /// it. A failed lookup keeps the old address.
    async fn refresh_address(&mut self) {
        let Some(resolver) = self.params.resolver.as_mut() else {
            return;
        };
        let Ok(Ok(addr)) = timeout(RESOLVE_TIMEOUT, resolver()).await else {
            return;
        };
        if addr != self.params.addr {
            let family_changed = addr.is_ipv4() != self.params.addr.is_ipv4();
            self.params.addr = addr;
            if family_changed {
                // the socket must be of the same family as the address
                self.rebind();
            }
        }
    }

    /// Makes an attempt to connect that a change of the network cuts short and starts again.
    async fn establish_watched(&mut self, hello: &Hello) -> Attempt {
        let mut net = self.net.take();
        let attempt = loop {
            tokio::select! {
                attempt = self.establish(hello) => break attempt,
                cause = next_cause(&mut net) => match cause {
                    Some(_) => self.rebind(),
                    None => net = None,
                },
            }
        };
        self.net = net;
        attempt
    }

    /// Connects, sends the hello, and resends the input that the server has not consumed.
    async fn establish(&mut self, hello: &Hello) -> Attempt {
        let attempt = timeout(self.params.attempt_timeout, async {
            let connecting = self.endpoint.connect(self.params.addr, SERVER_NAME).ok()?;
            let conn = connecting.await.ok()?;
            let (send, recv) = conn.open_bi().await.ok()?;
            let (out, out_rx) = mpsc::unbounded_channel();
            let writer = tokio::spawn(write_frames(send, out_rx));
            let mut link = Link {
                conn,
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
            // nobody has started the shell yet, so this client does it
            Ok(Some(ServerFrame::Error(ErrorCode::NotStarted)))
                if matches!(hello, Hello::Attach { .. }) && !self.started =>
            {
                self.params.attach = false;
                return Attempt::Retry;
            }
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
        if matches!(hello, Hello::Attach { .. }) {
            // a client without history takes over the offsets of the server
            self.output_offset = welcome.output_offset;
            self.input = ReplayBuffer::starting_at(INPUT_BUFFER, welcome.input_offset);
        }
        if check_welcome(hello, welcome, self.input.start(), self.input.end()).is_err() {
            return Attempt::Fatal("the server answered with unexpected offsets".to_owned());
        }
        if self.input.ack(welcome.input_offset).is_err() {
            return Attempt::Fatal("the server consumed input that was never sent".to_owned());
        }
        // The server forgot output while the client was away. The stream goes on at the new
        // offset, and the gap is shown in the output where it happened, with its exact size.
        if let Hello::Resume { output_offset, .. } = hello
            && welcome.output_offset > *output_offset
        {
            self.output_offset = welcome.output_offset;
            self.note_gap(welcome.output_offset - output_offset).await;
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
                cause = next_cause(&mut self.net) => match cause {
                    // the network is different now, so waiting longer makes no sense
                    Some(_) => {
                        self.rebind();
                        return Ok(None);
                    }
                    None => self.net = None,
                },
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
        // a change of the network that is waiting for the notifications to stop
        let mut change: Option<(Cause, tokio::time::Instant)> = None;
        // when the server has to have answered after the client moved to a new path
        let mut probe: Option<tokio::time::Instant> = None;
        let mut resizes = Debounce::new(RESIZE_QUIET);
        loop {
            // two bytes of reserve: the escape can turn one typed byte into two
            let room = self.input.free().saturating_sub(2).min(self.chunk.len());
            let change_at = change.map(|(_, at)| at);
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
                    probe = None;
                    if std::mem::take(&mut stalled) {
                        self.clear_status().await;
                    }
                    match frame {
                        ServerFrame::Output(data) => {
                            if self.io.output.write_all(&data).await.and(self.io.output.flush().await).is_err() {
                                return Err("cannot write to the terminal".to_owned());
                            }
                            self.tracker.feed(&data);
                            // a status that was held back because of a sequence can go out now
                            self.sync_status(false).await;
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
                    resizes.push(size, tokio::time::Instant::now());
                }
                _ = async { tokio::time::sleep_until(resizes.due().expect("guarded")).await }, if resizes.due().is_some() => {
                    if let Some(size) = resizes.take(tokio::time::Instant::now()) {
                        link.send(ClientFrame::Resize(size));
                    }
                }
                cause = next_cause(&mut self.net) => match cause {
                    Some(cause) => {
                        let strongest = change.map_or(cause, |(old, _)| old.max(cause));
                        change = Some((strongest, tokio::time::Instant::now() + CHANGE_QUIET));
                    }
                    None => self.net = None,
                },
                _ = async { tokio::time::sleep_until(change_at.expect("guarded")).await }, if change_at.is_some() => {
                    let (cause, _) = change.take().expect("guarded");
                    self.rebind();
                    link.send(ClientFrame::Ping);
                    let wait = if cause == Cause::Woke { PROBE_AFTER_WAKE } else { PROBE_AFTER_CHANGE };
                    probe = Some(tokio::time::Instant::now() + wait);
                }
                _ = async { tokio::time::sleep_until(probe.expect("guarded")).await }, if probe.is_some() => {
                    // the new path did not bring an answer, so the old connection is not worth keeping
                    return Ok(Pumped::Lost);
                }
                _ = ping.tick() => {
                    link.send(ClientFrame::Ping);
                    let silent = link.last_heard.elapsed();
                    let stall_after = (link.conn.rtt() * 4).clamp(STALL_MIN, STALL_MAX);
                    if silent > stall_after {
                        stalled = true;
                        // the number changes in steps of five seconds, so the title does not flicker
                        let text = if silent < Duration::from_secs(10) {
                            "no response from the server".to_owned()
                        } else {
                            format!("no response from the server for {}s", silent.as_secs() / 5 * 5)
                        };
                        self.show_status(&text).await;
                    }
                }
            }
        }
    }
}
