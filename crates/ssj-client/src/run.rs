//! The session of the client: connect, pass bytes between terminal and server, and reconnect.

use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::pin::Pin;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use quinn::Endpoint;
use ssj_net::{Identity, SERVER_NAME, client_tls, quic_client};
use ssj_proto::bootstrap::{CHOICE_EXIT_CODE, Connect, Prestart, sanitize};
use ssj_proto::compress::{Compressor, inflate};
use ssj_proto::control::{Reply, SessionInfo, SessionState, parse_reply};
use ssj_proto::frame::MAX_DATA_LEN;
use ssj_proto::resume::check_welcome;
use ssj_proto::{
    ClientFrame, Env, EnvFilter, ErrorCode, ExitStatus, Hello, Peek, ReplayBuffer, ServerFrame,
    WindowSize, decode_server, peek_server,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{interval, sleep, timeout};

use crate::agent::Forward;
use crate::bootstrap::{Remote, StartError, run_remote, start_server};
use crate::cli::{Args, Command, LocalEcho, Transport};
use crate::config::Config;
use crate::debounce::Debounce;
use crate::echo::Echo;
use crate::install::{Decision, confirm, decide, install, interactive};
use crate::netwatch::Cause;
use crate::sessions_cache;
use crate::status::{LineStatus, Status, TitleStatus, dropped_notice};
use crate::terminal::{RawMode, ask, exit_on_interrupt, window_size};
use crate::tracker::Tracker;
use crate::tunnel::{Kind, SshConfig};
use crate::vlog;

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
/// Head start of QUIC over the stream transport when both are tried. A QUIC handshake needs one
/// round trip, so a transport that has not answered by then is likely blocked.
const RACE_STAGGER: Duration = Duration::from_millis(300);
/// How long after the last debug line on the terminal the screen is drawn again.
const REDRAW_AFTER_LOG: Duration = Duration::from_millis(400);
/// Time QUIC gets to open on a new network while the session runs over a stream transport.
const UPGRADE_TIMEOUT: Duration = Duration::from_millis(1500);
/// Failed attempts on a tier that worked before, until the next tier is tried.
const TIER_PATIENCE: usize = 3;
/// Time QUIC gets before the first connection when a stream transport runs beside it.
/// Later it may be slow to return, and gets the full time.
pub const FIRST_ATTEMPT_TIMEOUT: Duration = Duration::from_millis(1500);
/// A stream that stays silent this long is given up. The server answers a ping every second.
pub const STREAM_DEAD_AFTER: Duration = Duration::from_secs(10);
/// Longest note about a failed attempt in the status message.
const NOTE_CHARS: usize = 60;
/// A new lookup of the server address happens before every attempt after this many failed ones.
const RESOLVE_EVERY: usize = 3;
/// Time a lookup may take before the old address is kept.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(2);
/// A resize is sent when no newer one has come for this long.
const RESIZE_QUIET: Duration = Duration::from_millis(50);
const PING_EVERY: Duration = Duration::from_secs(1);
/// Output that is acknowledged at once. Less waits for a frame that goes out anyway.
const ACK_BYTES: u64 = 4096;
/// Input is compressed only while at least this many bytes wait for the server to take them.
const COMPRESS_BACKLOG: u64 = 256 * 1024;
/// Chunks shorter than this are not worth compressing.
const COMPRESS_MIN_CHUNK: usize = 1024;
/// Most output that is written to the terminal in one go.
const OUTPUT_BATCH: usize = 256 * 1024;
/// How long a status waits for a sequence of the remote side to end before it cancels it.
const SEQUENCE_PATIENCE: Duration = Duration::from_secs(2);
/// The client says that the server is silent after four round trips, but not sooner than this
/// and not later than the maximum, so that a good link reports a stall quickly and a slow link
/// does not report one all the time.
const STALL_MIN: Duration = Duration::from_secs(2);
const STALL_MAX: Duration = Duration::from_secs(10);
/// A stream that has heard nothing for this long gets a spare: a second connection is opened
/// beside it, and the session moves to whichever works. Giving the stream up only after
/// [`STREAM_DEAD_AFTER`] would leave a flaky link (a train) without a session for that long, and
/// the stream itself may be in the long backoff of TCP when the network is back.
const SPARE_AFTER: Duration = Duration::from_secs(3);
/// Time a spare gets to open. A connection that was started on a dead path has lost its first
/// packets and retries them after 1 and 3 seconds, and then needs the login: it opens about 6
/// seconds after it was started, so a shorter time would end the spares that are about to work.
/// The fresh ones that come every second find a returning network sooner.
const SPARE_TIMEOUT: Duration = Duration::from_secs(10);
/// Spares that open at the same time. A connection started on a dead path loses its first packets
/// and may not open for seconds, so a new one starts every second while the stream lags, and the
/// first to open wins (the same idea as asking two servers and taking the faster answer).
const SPARES_AT_ONCE: usize = 6;
/// Pause after a spare that failed outright, so that a broken login does not become a loop.
const SPARE_PAUSE: Duration = Duration::from_secs(2);
/// A change of network produces many notifications. One reaction follows when they stop.
const CHANGE_QUIET: Duration = Duration::from_millis(300);
/// Time the server gets to answer after the client moved to a new network path, before the client
/// starts a new connection. A wake from sleep leaves less hope for the old path.
const PROBE_AFTER_CHANGE: Duration = Duration::from_secs(1);
const PROBE_AFTER_WAKE: Duration = Duration::from_secs(1);

/// How the session ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Finished {
    /// The shell ended with this exit code.
    Exited(u8),
    /// The user left on purpose and the session keeps running.
    Detached,
    /// The terminal was closed or `ssj` was told to end, and the server was asked to end the
    /// session (it could not be, if the connection was down).
    HungUp,
}

/// Looks up the address of the server again.
pub type Resolver =
    Box<dyn FnMut() -> Pin<Box<dyn Future<Output = Result<SocketAddr, String>> + Send>> + Send>;

/// Read half of a byte stream.
pub type Reader = Box<dyn AsyncRead + Send + Unpin>;
/// Write half of a byte stream.
pub type Writer = Box<dyn AsyncWrite + Send + Unpin>;

/// An open byte stream to the server on a transport other than QUIC.
pub struct Stream {
    pub reader: Reader,
    pub writer: Writer,
    /// Something that must live as long as the stream, such as the `ssh` process behind it.
    pub guard: Option<Box<dyn Send + Sync>>,
}

/// Opens a new stream to the server.
pub type Dial =
    Box<dyn FnMut() -> Pin<Box<dyn Future<Output = Result<Stream, String>> + Send>> + Send>;

/// One way to reach the server.
pub enum Tier {
    /// QUIC over UDP.
    Udp {
        addr: SocketAddr,
        /// Finds the address again when it may have changed. `None` keeps `addr` for good.
        resolver: Option<Resolver>,
    },
    /// A byte stream with TLS (TCP through `ssh -W`) or without (the `ssh` channel itself).
    Stream { name: &'static str, dial: Dial },
}

impl Tier {
    fn name(&self) -> &'static str {
        match self {
            Self::Udp { .. } => "udp",
            Self::Stream { name, .. } => name,
        }
    }
}

/// Everything needed to talk to a started server.
pub struct Params {
    pub identity: Identity,
    pub connect: Connect,
    /// The ways to reach the server, in the order to try them.
    pub tiers: Vec<Tier>,
    /// Time for one attempt to connect and to get the welcome.
    pub attempt_timeout: Duration,
    /// Time the first tier gets in the first connection, before the next one is tried.
    pub first_attempt_timeout: Duration,
    /// Silence after which a stream (TCP or `ssh` channel) is given up.
    pub stream_dead_after: Duration,
    pub env: Env,
    /// A control byte that detaches wherever it is typed.
    pub detach_key: Option<u8>,
    /// A closed terminal or `SIGHUP` detaches and does not end the session: `ssj` runs inside an
    /// `ssh` session, whose end is more likely a lost connection than a wish.
    pub hangup_detaches: bool,
    /// Join a running session instead of starting one.
    pub attach: bool,
    /// Show typed characters before the server echoes them (see [`crate::echo`]).
    pub local_echo: bool,
    /// Compress input while the link is the limit (the server decides for its output).
    pub compress: bool,
    /// The socket of the local ssh agent, to offer it to the session.
    pub agent: Option<PathBuf>,
    /// What the line that says the session began (`[started session ...]`) names, or `None` for no
    /// line (`-q`, or no terminal).
    pub announce: Option<Announce>,
}

/// What the first line of a session names besides its id.
#[derive(Clone, Debug)]
pub struct Announce {
    pub destination: String,
    /// The name of the session, when it has one.
    pub name: Option<String>,
    /// Whether the server made the session for this client. A server that does not say counts as
    /// no: the line then says `attached to`.
    pub started: bool,
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
    /// The terminal was closed (SIGHUP) or `ssj` was told to end (SIGTERM). A channel that never
    /// sends is fine.
    pub hangups: mpsc::UnboundedReceiver<()>,
}

/// Runs the client for a parsed command line and returns the exit code.
pub fn run(args: Args) -> ExitCode {
    if let Err(err) = crate::log::init(args.verbose, args.log_file.as_deref()) {
        eprintln!("ssj: cannot open the log file: {err}");
        return ExitCode::from(1);
    }
    vlog!(1, "ssj {} starting", env!("CARGO_PKG_VERSION"));
    let runtime = match tokio::runtime::Builder::new_current_thread()
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
    // the runtime to finish would hang forever.
    runtime.shutdown_background();
    ExitCode::from(code)
}

/// The agent socket to forward, if the user asked for it.
fn agent_socket(args: &Args) -> Result<Option<PathBuf>, String> {
    if !args.forward_agent {
        return Ok(None);
    }
    match std::env::var_os("SSH_AUTH_SOCK").map(PathBuf::from) {
        Some(path) if path.is_absolute() => Ok(Some(path)),
        _ => Err("agent forwarding needs SSH_AUTH_SOCK, and there is no agent running".to_owned()),
    }
}

/// Selected variables of the local environment, see [`send_patterns`].
fn local_env(args: &Args, ssh: Option<&SshConfig>) -> Result<Env, String> {
    // a real answer has the host name; without one there are no patterns of ssh, which is not the
    // same as an answer without `SendEnv` lines.
    let from_ssh = ssh
        .filter(|config| config.hostname.is_some())
        .map(|config| config.send_env.clone());
    vlog!(2, "SendEnv of ssh: {from_ssh:?}");
    let filter = send_patterns(&args.send_env, &args.send_env_add, from_ssh)
        .map_err(|e| format!("--send-env: {e:?}"))?;
    let vars = std::env::vars_os()
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)));
    selected_env(vars, &filter)
}

/// The patterns of the variables to send: what `ssh` sends for the destination (`from_ssh`), or
/// `TERM`, `LANG`, and `LC_*` when `ssh` gave no answer, then the `send_env` of the config file and
/// the `--send-env` patterns on top.
fn send_patterns(
    config: &EnvFilter,
    added: &[String],
    from_ssh: Option<Vec<String>>,
) -> Result<EnvFilter, ssj_proto::PatternError> {
    let own = config.patterns().iter().chain(added).cloned();
    let fallback = EnvFilter::default_patterns();
    match from_ssh {
        // too many patterns with those of ssh: the own ones are the user's choice.
        Some(patterns) => EnvFilter::new(patterns)
            .and_then(|filter| filter.extended(own.clone()))
            .or_else(|_| fallback.extended(own)),
        None => fallback.extended(own),
    }
}

/// The variables that `filter` selects, and always `TERM`: the shell needs the terminal type
/// whatever else the user sends, as with `ssh`, which sends it with the request for the terminal.
/// Without it the shell takes the terminal for a dumb one and draws its prompt wrongly.
fn selected_env(
    vars: impl IntoIterator<Item = (String, String)>,
    filter: &EnvFilter,
) -> Result<Env, String> {
    let vars = vars
        .into_iter()
        .filter(|(name, _)| name == "TERM" || filter.matches(name))
        .collect();
    Env::new(vars).map_err(|e| format!("the selected environment is not valid: {e:?}"))
}

/// The address that binds a UDP socket of the same family as `peer`.
fn wildcard(peer: &SocketAddr) -> SocketAddr {
    if peer.is_ipv4() {
        SocketAddr::from(([0, 0, 0, 0], 0))
    } else {
        SocketAddr::from(([0u16; 8], 0))
    }
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
    exit_on_interrupt();
    let config = match Config::load() {
        Ok(config) => config,
        Err(message) => {
            eprintln!("ssj: {message}");
            return 2;
        }
    };
    args.apply_config(&config);
    // the server compresses its output when the user asked for compression (or for none)
    if let Some(on) = args.compress {
        args.server_args.extend([
            "--compress".to_owned(),
            if on { "auto" } else { "off" }.to_owned(),
        ]);
    }
    if let Some(command) = &args.remote_command {
        // `ssh host command` runs the command in a shell, and so does this: the login shell of the
        // host with `-c`.
        for word in [
            "--shell-arg",
            "-l",
            "--shell-arg",
            "-c",
            "--shell-arg",
            command,
        ] {
            args.server_args.push(word.to_owned());
        }
    }
    if let Some(choice) = args.congestion {
        ssj_net::set_congestion(choice);
        args.server_args
            .extend(["--congestion".to_owned(), choice.as_str().to_owned()]);
    }
    if args.batch {
        // nobody can answer a prompt of a background refresh, and nobody waits for it.
        args.ssh_command
            .extend(["-o", "BatchMode=yes", "-o", "ConnectTimeout=2"].map(str::to_owned));
    }
    match args.command.clone() {
        Command::Complete { host, connected } => complete(host.as_deref(), connected),
        Command::Connect => {
            // a name or `auto_attach` may mean a session that runs already; the server decides,
            // and the client starts with an attach, which a new session answers with `NotStarted`.
            let mut extra = Vec::new();
            if let Some(name) = &args.name {
                extra.extend(["--name".to_owned(), name.as_str().to_owned()]);
            } else if args.auto_attach {
                extra.push("--reuse".to_owned());
            }
            let attach = !extra.is_empty();
            connect_session(args, &config, &extra, attach, true).await
        }
        Command::Attach(id) => {
            let mut extra = vec!["--attach".to_owned()];
            extra.extend(id);
            connect_session(args, &config, &extra, true, false).await
        }
        Command::List => list_sessions(&args).await,
        Command::Kill(ids) => kill_session(&args, ids).await,
        Command::Detach(ids) => detach_session(&args, ids).await,
        Command::Edit => crate::edit::run(&args).await,
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
    prestart: Option<&Prestart>,
) -> Result<Connect, String> {
    vlog!(
        1,
        "starting the server on {} through {}",
        args.destination,
        args.ssh_command[0]
    );
    // with several sessions to join and a person at the terminal, the server hands the choice back.
    let mut extra = extra.to_vec();
    // `--reuse` may start a session, and a bare `--attach` cannot name one.
    let reuse = extra.iter().any(|word| word == "--reuse");
    let choosing = (reuse || extra == ["--attach"]) && interactive() && !args.batch;
    if choosing {
        extra.push("--choose".to_owned());
    }
    let extra = &extra[..];
    match start_server(args, fingerprint, extra, prestart).await {
        Ok(connect) => Ok(connect),
        Err(StartError::Failed(message)) => Err(message),
        Err(StartError::Choose(sessions)) => {
            choose_and_start(args, fingerprint, prestart, sessions, reuse).await
        }
        // only an explicit `ssj attach` has to find a session; `--name` and `--auto-attach` start
        // one when there is none, and so may install the server.
        Err(StartError::Missing) if extra.first().is_some_and(|word| word == "--attach") => {
            Err(format!(
                "ssj-server is not installed on {}, so no session runs there",
                args.destination
            ))
        }
        Err(StartError::Missing) => {
            vlog!(1, "the server is not installed on the host");
            let policy = args.auto_install.unwrap_or(config.auto_install);
            match decide(policy, &args.destination, interactive(), confirm) {
                Decision::Skip(message) => Err(message),
                Decision::Install => {
                    let path = install(args).await?;
                    eprintln!("ssj: installed {path}");
                    match start_server(args, fingerprint, extra, prestart).await {
                        Ok(connect) => Ok(connect),
                        Err(StartError::Missing) => {
                            Err("the server is still missing after the installation".to_owned())
                        }
                        Err(StartError::Failed(message)) => Err(message),
                        Err(StartError::Choose(_)) => {
                            Err("the host has no session to choose from".to_owned())
                        }
                    }
                }
            }
        }
    }
}

/// One line of the menu for a session.
fn session_line(s: &SessionInfo) -> String {
    format!(
        "{}  {:<12}  {:>4}  {}",
        s.id.as_str(),
        state_word(s.state),
        format_age(s.age_secs),
        s.name.as_ref().map_or("", |name| name.as_str())
    )
}

/// What the user picked in the menu of sessions.
enum Picked {
    Cancelled,
    /// The line "start a new session".
    New,
    Session(String),
}

/// Shows the sessions, the youngest first (most likely the one that was just left), and asks for
/// one. With `allow_new` the last line starts a new session.
async fn pick_session(title: String, mut sessions: Vec<SessionInfo>, allow_new: bool) -> Picked {
    sessions.sort_by_key(|s| s.age_secs);
    let mut items: Vec<String> = sessions.iter().map(session_line).collect();
    if allow_new {
        items.push("start a new session".to_owned());
    }
    let picked = tokio::task::spawn_blocking(move || {
        ask(dialoguer::Select::new()
            .with_prompt(title)
            .items(&items)
            .default(0)
            .interact_opt())
    })
    .await
    .ok()
    .flatten();
    match picked {
        None => Picked::Cancelled,
        Some(at) if at < sessions.len() => Picked::Session(sessions[at].id.as_str().to_owned()),
        Some(_) => Picked::New,
    }
}

/// Asks which of the sessions to join, or whether to start another, and starts the server for
/// that answer.
async fn choose_and_start(
    args: &Args,
    fingerprint: &[u8; 32],
    prestart: Option<&Prestart>,
    sessions: Vec<SessionInfo>,
    allow_new: bool,
) -> Result<Connect, String> {
    let title = format!(
        "ssj: {} sessions on {} {}. Which one?",
        sessions.len(),
        args.destination,
        if allow_new {
            "have no client"
        } else {
            "are running"
        }
    );
    let extra = match pick_session(title, sessions, allow_new).await {
        Picked::Cancelled => return Err("cancelled".to_owned()),
        Picked::Session(id) => {
            vlog!(1, "joining the session {id}");
            vec!["--attach".to_owned(), id]
        }
        Picked::New => {
            vlog!(1, "starting a new session");
            Vec::new()
        }
    };
    match start_server(args, fingerprint, &extra, prestart).await {
        Ok(connect) => Ok(connect),
        Err(StartError::Failed(message)) => Err(message),
        Err(StartError::Missing | StartError::Choose(_)) => {
            Err("the session is gone; run the command again".to_owned())
        }
    }
}

/// Runs `--kill` or `--detach` on the host. Without an id, and with a person at the terminal,
/// several sessions are offered in a menu.
async fn run_choosing(
    args: &Args,
    flag: &str,
    id: Option<String>,
    question: &str,
) -> Result<Remote, String> {
    let mut extra = vec![flag.to_owned()];
    extra.extend(id.clone());
    // a person is asked when there is no id, and when the id or name fits several sessions
    let choosing = interactive() && !args.batch;
    if choosing {
        extra.push("--choose".to_owned());
    }
    let mut remote = run_remote(args, &extra, "").await?;
    if choosing && remote.status.code() == Some(i32::from(CHOICE_EXIT_CODE)) {
        let mut sessions: Vec<SessionInfo> = remote
            .stdout
            .split(|b| *b == b'\n')
            .filter_map(|line| match parse_reply(line) {
                Ok(Reply::Info(info)) => Some(info),
                _ => None,
            })
            .collect();
        if flag == "--detach" {
            // only a session with a client can be detached.
            sessions.retain(|s| s.state == SessionState::Connected);
        }
        let id = match sessions.len() {
            0 => return Err("no client is connected".to_owned()),
            1 => sessions[0].id.as_str().to_owned(),
            n => {
                let title = format!("ssj: {n} sessions on {}. {question}", args.destination);
                match pick_session(title, sessions, false).await {
                    Picked::Session(id) => id,
                    _ => return Err("cancelled".to_owned()),
                }
            }
        };
        remote = run_remote(args, &[flag.to_owned(), id], "").await?;
    }
    Ok(remote)
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

/// The sessions as a JSON array, one object each.
pub fn sessions_json(infos: &[SessionInfo]) -> String {
    // the id and the name are restricted to characters that need no escaping.
    let items: Vec<String> = infos
        .iter()
        .map(|info| {
            let name = info
                .name
                .as_ref()
                .map_or("null".to_owned(), |name| format!("\"{}\"", name.as_str()));
            format!(
                "{{\"id\":\"{}\",\"name\":{name},\"state\":\"{}\",\"age_secs\":{},\"protocol\":{}}}",
                info.id.as_str(),
                state_word(info.state),
                info.age_secs,
                info.protocol
            )
        })
        .collect();
    format!("[{}]", items.join(","))
}

/// Prints the cached names for tab completion and refreshes old lists in the background.
fn complete(host: Option<&str>, connected: bool) -> u8 {
    let Some(dir) = sessions_cache::dir() else {
        return 0;
    };
    let names = if connected {
        sessions_cache::connected_names(&dir, host)
    } else {
        sessions_cache::names(&dir, host)
    };
    for name in names {
        println!("{name}");
    }
    let Ok(exe) = std::env::current_exe() else {
        return 0;
    };
    for origin in sessions_cache::stale_origins(&dir, sessions_cache::REFRESH_AFTER) {
        let target = match origin.port {
            Some(port) if origin.destination.contains(':') => {
                // an IPv6 address needs brackets in front of a port.
                let (user, host) = origin
                    .destination
                    .rsplit_once('@')
                    .map_or(("", origin.destination.as_str()), |(u, h)| (u, h));
                let user = if user.is_empty() {
                    String::new()
                } else {
                    format!("{user}@")
                };
                format!("ssh://{user}[{host}]:{port}")
            }
            Some(port) => format!("ssh://{}:{port}", origin.destination),
            None => origin.destination.clone(),
        };
        // the list writes the cache; the process is not waited for.
        let _ = std::process::Command::new(&exe)
            .args(["--batch", "list", &target])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
    0
}

fn remember(origin: &sessions_cache::Origin, entries: &[(String, bool)]) {
    if let Some(dir) = sessions_cache::dir() {
        // a cache that cannot be written only costs the completion.
        let _ = sessions_cache::store_states(&dir, origin, entries);
    }
}

async fn list_sessions(args: &Args) -> u8 {
    // one refresh per host at a time; a second one has nothing to add.
    let _refresh = if args.batch {
        match sessions_cache::dir()
            .and_then(|dir| sessions_cache::Refresh::acquire(&dir, &args.destination))
        {
            Some(lock) => Some(lock),
            None => return 0,
        }
    } else {
        None
    };
    let origin = sessions_cache::Origin {
        destination: args.destination.clone(),
        port: args.ssh_port,
    };
    let remote = match run_remote(args, &["--list".to_owned()], "").await {
        Ok(remote) if remote.server_missing() => {
            remember(&origin, &[]);
            if !args.batch {
                if args.json {
                    println!("[]");
                } else {
                    println!(
                        "no sessions (ssj-server is not installed on {})",
                        args.destination
                    );
                }
            }
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
    let mut names = Vec::new();
    for info in &infos {
        let connected = info.state == SessionState::Connected;
        names.extend(
            info.name
                .as_ref()
                .map(|name| (name.as_str().to_owned(), connected)),
        );
        names.push((info.id.as_str().to_owned(), connected));
    }
    remember(&origin, &names);
    if args.batch {
        return 0;
    }
    if args.json {
        println!("{}", sessions_json(&infos));
        return 0;
    }
    if infos.is_empty() {
        println!("no sessions");
    }
    // a header helps a person and breaks a script, so only a terminal gets one.
    let header = std::io::IsTerminal::is_terminal(&std::io::stdout());
    for line in list_lines(&infos, header) {
        println!("{line}");
    }
    0
}

/// The lines of `ssj list`, with a header line if `header` is set.
pub fn list_lines(infos: &[SessionInfo], header: bool) -> Vec<String> {
    let id_width = infos
        .iter()
        .map(|i| i.id.as_str().len())
        .max()
        .unwrap_or(0)
        .max(2);
    let row = |id: &str, state: &str, age: &str, protocol: &str, name: &str| {
        format!("{id:<id_width$}  {state:<12}  {age:>4}  {protocol:<10}  {name}")
    };
    let mut lines = Vec::new();
    if header && !infos.is_empty() {
        lines.push(row("ID", "STATE", "AGE", "PROTOCOL", "NAME"));
    }
    for info in infos {
        lines.push(row(
            info.id.as_str(),
            state_word(info.state),
            &format_age(info.age_secs),
            &format!("protocol {}", info.protocol),
            info.name.as_ref().map_or("-", |name| name.as_str()),
        ));
    }
    lines
}

/// The sessions of the host, as `ssj list` has them.
async fn fetch_sessions(args: &Args) -> Result<Vec<SessionInfo>, String> {
    let remote = run_remote(args, &["--list".to_owned()], "").await?;
    if remote.server_missing() {
        return Ok(Vec::new());
    }
    if !remote.status.success() {
        return Err(format!(
            "could not list the sessions ({}): {}",
            remote.status,
            remote.error_text()
        ));
    }
    Ok(remote
        .stdout
        .split(|b| *b == b'\n')
        .filter_map(|line| match parse_reply(line) {
            Ok(Reply::Info(info)) => Some(info),
            _ => None,
        })
        .collect())
}

/// What `ssj kill HOST` does without an id: the only session is ended after a question (unless
/// `-f` is given), and several sessions go into a menu, where the choice is the consent. Returns
/// the id, or the message that ends the command.
async fn session_to_kill(args: &Args) -> Result<String, String> {
    if !interactive() {
        // the server ends the only session, and refuses when there are several, and says so when
        // there is none: only a session that would be ended without a word needs the flag
        if args.force || fetch_sessions(args).await?.is_empty() {
            return Ok(String::new());
        }
        return Err("there is no terminal to ask: name the session, or use -f".to_owned());
    }
    let mut sessions = fetch_sessions(args).await?;
    sessions.retain(|s| s.protocol == ssj_proto::PROTOCOL);
    sessions.sort_by_key(|s| s.age_secs);
    match sessions.len() {
        0 => Err("no running session".to_owned()),
        1 => {
            let s = &sessions[0];
            if !args.force {
                let what = s.name.as_ref().map_or_else(
                    || s.id.as_str().to_owned(),
                    |name| format!("{} ({})", name.as_str(), s.id.as_str()),
                );
                let question = format!(
                    "End the session {what} on {} ({}, {})?",
                    args.destination,
                    state_word(s.state),
                    format_age(s.age_secs)
                );
                if !confirm(&question) {
                    return Err("not ended".to_owned());
                }
            }
            Ok(s.id.as_str().to_owned())
        }
        n => {
            let title = format!(
                "ssj: {n} sessions on {}. Which one to end?",
                args.destination
            );
            match pick_session(title, sessions, false).await {
                Picked::Session(id) => Ok(id),
                _ => Err("cancelled".to_owned()),
            }
        }
    }
}

async fn kill_session(args: &Args, mut ids: Vec<String>) -> u8 {
    if ids.is_empty() {
        match session_to_kill(args).await {
            Ok(id) if !id.is_empty() => ids.push(id),
            Ok(_) => {}
            Err(message) => {
                eprintln!("ssj: {message}");
                return 1;
            }
        }
    }
    let count = ids.len().max(1);
    let result = if ids.len() > 1 {
        // one call to the host ends them all.
        let mut extra = vec!["--kill".to_owned()];
        extra.extend(ids);
        run_remote(args, &extra, "").await
    } else {
        run_choosing(args, "--kill", ids.pop(), "Which one to end?").await
    };
    match result {
        Ok(remote) if remote.server_missing() => {
            eprintln!(
                "ssj: no running session (ssj-server is not installed on {})",
                args.destination
            );
            1
        }
        Ok(remote) if remote.status.success() => {
            if count > 1 {
                println!("{count} sessions ended");
            } else {
                println!("session ended");
            }
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

async fn detach_session(args: &Args, mut ids: Vec<String>) -> u8 {
    let count = ids.len().max(1);
    let result = if ids.len() > 1 {
        // one call to the host detaches them all.
        let mut extra = vec!["--detach".to_owned()];
        extra.extend(ids);
        run_remote(args, &extra, "").await
    } else {
        run_choosing(args, "--detach", ids.pop(), "Which one to detach?").await
    };
    match result {
        Ok(remote) if remote.server_missing() => {
            eprintln!(
                "ssj: no running session (ssj-server is not installed on {})",
                args.destination
            );
            1
        }
        Ok(remote) if remote.status.success() => {
            if count > 1 {
                println!("{count} clients detached; the sessions keep running");
            } else {
                println!("client detached; the session keeps running");
            }
            0
        }
        Ok(remote) => {
            eprintln!("ssj: could not detach the session: {}", remote.error_text());
            1
        }
        Err(message) => {
            eprintln!("ssj: {message}");
            1
        }
    }
}

/// Reports SIGHUP (the terminal went away) and SIGTERM (`kill`): `ssj` then ends the session, as
/// `ssh` would end its shell. Losing the connection is a different thing and keeps the session.
fn hangup_signals() -> mpsc::UnboundedReceiver<()> {
    let (tx, rx) = mpsc::unbounded_channel();
    for kind in [SignalKind::hangup(), SignalKind::terminate()] {
        if let Ok(mut signal) = signal(kind) {
            let tx = tx.clone();
            tokio::spawn(async move {
                while signal.recv().await.is_some() {
                    if tx.send(()).is_err() {
                        break;
                    }
                }
            });
        }
    }
    rx
}

/// Looks up the address that UDP will use, while the `ssh` of the bootstrap is still busy, so that
/// the start does not wait for it afterwards.
async fn udp_address(args: &Args, config: Option<&SshConfig>) -> Option<IpAddr> {
    if !matches!(args.transport, Transport::Auto | Transport::Udp) {
        return None;
    }
    let host = udp_host(args, config);
    let address = resolve(&host, 0).await.ok().map(|addr| addr.ip());
    vlog!(2, "the address of {host} for UDP: {address:?}");
    address
}

/// The address of the server: the one given, else the `HostName` of the `ssh` configuration (the
/// destination may be an alias), else the destination itself.
fn udp_host(args: &Args, config: Option<&SshConfig>) -> String {
    args.udp_host
        .clone()
        .or_else(|| config.and_then(|c| c.hostname.clone()))
        .unwrap_or_else(|| args.host().to_owned())
}

async fn build_tiers(
    args: &Args,
    identity: &Identity,
    connect: &Connect,
    config: Option<&SshConfig>,
    address: Option<IpAddr>,
) -> Result<Vec<Tier>, String> {
    let choice = args.transport;
    // UDP does not travel through a jump host, so it is not even tried there.
    let udp_possible = matches!(choice, Transport::Auto | Transport::Udp) && connect.udp.is_some();
    let config = config.filter(|_| udp_possible);
    let jump = choice == Transport::Auto && config.is_some_and(|c| c.jump);
    let order =
        crate::tunnel::tier_order(choice, jump, connect.udp.is_some(), connect.tcp.is_some());
    let mut tiers = Vec::new();
    let mut notes = Vec::new();
    for kind in order {
        match kind {
            Kind::Udp => {
                let port = connect.udp.expect("offered");
                // the address of the server: the one given, else the `HostName` of the `ssh`
                // configuration (the destination may be an alias), else the destination itself.
                let host = udp_host(args, config);
                let found = match address {
                    Some(ip) => Ok(SocketAddr::new(ip, port)),
                    None => resolve(&host, port).await,
                };
                match found {
                    Ok(addr) => tiers.push(Tier::Udp {
                        addr,
                        resolver: Some(Box::new(move || {
                            let host = host.clone();
                            Box::pin(async move { resolve(&host, port).await })
                        })),
                    }),
                    Err(message) => notes.push(message),
                }
            }
            Kind::Tcp => tiers.push(crate::tunnel::tcp_tier(
                args,
                ssj_net::tls_connector(identity, connect.fingerprint),
                connect.tcp.expect("offered"),
                ATTEMPT_TIMEOUT,
            )),
            Kind::Stdio => tiers.push(crate::tunnel::stdio_tier(
                args,
                &connect.id,
                ATTEMPT_TIMEOUT,
            )),
        }
    }
    if tiers.is_empty() {
        match choice {
            Transport::Udp if connect.udp.is_none() => {
                notes.push("the server offers no UDP port".to_owned())
            }
            Transport::Tcp if connect.tcp.is_none() => {
                notes.push("the server offers no TCP port".to_owned())
            }
            _ => {}
        }
        return Err(notes.join("; "));
    }
    Ok(tiers)
}

/// With `fresh` the server may start the shell of a new session at once (see `Prestart`).
async fn connect_session(
    args: Args,
    config: &Config,
    extra: &[String],
    attach: bool,
    fresh: bool,
) -> u8 {
    let identity = Identity::generate();
    let prepared = async {
        let fingerprint = identity.fingerprint();
        let ssh = crate::tunnel::ssh_config(&args).await;
        vlog!(
            2,
            "ssh -G: HostName {:?}, jump host or proxy: {}",
            ssh.as_ref().and_then(|c| c.hostname.as_deref()),
            ssh.as_ref().is_some_and(|c| c.jump)
        );
        let env = local_env(&args, ssh.as_ref())?;
        let agent = agent_socket(&args)?;
        let prestart = fresh.then(|| Prestart {
            size: window_size(),
            env: env.clone(),
            agent: agent.is_some(),
        });
        let (connect, address) = tokio::join!(
            start_or_install(&args, config, &fingerprint, extra, prestart.as_ref()),
            udp_address(&args, ssh.as_ref())
        );
        let connect = connect?;
        let new = connect.new;
        vlog!(
            1,
            "the server runs: session {}, udp {:?}, tcp {:?}",
            connect.id.as_str(),
            connect.udp,
            connect.tcp
        );
        let tiers = build_tiers(&args, &identity, &connect, ssh.as_ref(), address).await?;
        vlog!(
            1,
            "transports in order: {}",
            tiers.iter().map(Tier::name).collect::<Vec<_>>().join(", ")
        );
        Ok::<_, String>((connect, new, env, tiers, agent))
    }
    .await;
    let (connect, new_session, env, tiers, agent) = match prepared {
        Ok(prepared) => prepared,
        Err(message) => {
            eprintln!("ssj: {message}");
            return 1;
        }
    };

    // the server says the name: it is not only the one the user typed (`ssj attach host` joins a
    // session whose name the user may not remember).
    let session_name = connect.name.as_ref().map(|name| name.as_str().to_owned());
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
        hangups: hangup_signals(),
        net: {
            let watch = crate::netwatch::start();
            vlog!(2, "network change sources: {:?}", watch.active);
            watch.events
        },
    };
    let result = run_session(
        Params {
            identity,
            connect,
            tiers,
            attempt_timeout: ATTEMPT_TIMEOUT,
            first_attempt_timeout: FIRST_ATTEMPT_TIMEOUT,
            stream_dead_after: STREAM_DEAD_AFTER,
            env,
            detach_key: args.detach_key,
            hangup_detaches: std::env::var_os("SSH_TTY").is_some(),
            attach,
            local_echo: raw.is_some() && args.local_echo == LocalEcho::Auto,
            compress: args.compress == Some(true),
            agent,
            announce: (raw.is_some() && !args.quiet).then(|| Announce {
                destination: args.destination.clone(),
                started: new_session,
                name: session_name,
            }),
        },
        io,
    )
    .await;
    drop(raw);

    match result {
        Ok(Finished::Exited(code)) => {
            notice(&args, &format!("[disconnected from {}]", args.destination));
            code
        }
        Ok(Finished::HungUp) => 129,
        Ok(Finished::Detached) => {
            let port = args.ssh_port.map(|p| format!(":{p}")).unwrap_or_default();
            say(
                &args,
                &format!(
                    "[detached from session; it keeps running: ssj attach {}{port}]",
                    args.destination
                ),
            );
            0
        }
        Err(message) => {
            eprintln!("ssj: {message}");
            1
        }
    }
}

/// Prints a line about the session, unless `-q` is given. After a hangup the terminal is gone, and
/// `eprintln!` would panic on the error, so the write ignores errors.
fn say(args: &Args, text: &str) {
    if !args.quiet {
        let _ = std::io::Write::write_fmt(&mut std::io::stderr(), format_args!("{text}\n"));
    }
}

/// Like [`say`], for the lines that only make sense for a person at a terminal: a script that
/// reads the output of a command does not want them.
fn notice(args: &Args, text: &str) {
    use std::io::IsTerminal;
    if std::io::stdin().is_terminal() {
        say(args, text);
    }
}

/// Bytes asked from the stream at a time.
const READ_SIZE: usize = 64 * 1024;

/// Reads server frames from a stream, keeping bytes of incomplete frames between calls.
struct FrameReader {
    recv: Reader,
    /// Received bytes. The ones before `start` are used already; they are dropped when more is
    /// read, once per read and not once per frame.
    buf: Vec<u8>,
    start: usize,
}

impl FrameReader {
    fn new(recv: Reader) -> Self {
        Self {
            recv,
            buf: Vec::with_capacity(2 * READ_SIZE),
            start: 0,
        }
    }

    fn pending(&self) -> &[u8] {
        &self.buf[self.start..]
    }

    fn consume(&mut self, used: usize) {
        self.start += used;
        if self.start == self.buf.len() {
            self.buf.clear();
            self.start = 0;
        }
    }

    /// Next frame, or `None` when the stream ended.
    async fn next(&mut self) -> Result<Option<ServerFrame>, String> {
        loop {
            match decode_server(self.pending()) {
                Ok(Some((frame, used))) => {
                    self.consume(used);
                    return Ok(Some(frame));
                }
                Ok(None) => {}
                Err(err) => return Err(format!("the server sent an invalid frame: {err:?}")),
            }
            if self.start > 0 {
                self.buf.drain(..self.start);
                self.start = 0;
            }
            // read straight into the spare room of the buffer. This future is dropped when
            // another branch of the `select!` wins, so the length of the buffer must only grow
            // when a read has completed: `read_buf` does that, and padding the buffer with zeros
            // before the read would leave the zeros in it.
            self.buf.reserve(READ_SIZE);
            match self.recv.read_buf(&mut self.buf).await {
                Ok(0) | Err(_) => return Ok(None),
                Ok(_) => {}
            }
        }
    }

    /// Appends the data of the next frame to `out` if it is a complete output frame that is
    /// waiting already. Any other frame stays where it is, for [`Self::next`].
    fn take_ready_output(&mut self, out: &mut Vec<u8>) -> bool {
        match peek_server(self.pending()) {
            Ok(Peek::Output { data, used }) => {
                let start = self.start;
                out.extend_from_slice(&self.buf[start + data.start..start + data.end]);
                self.consume(used);
                true
            }
            _ => false,
        }
    }

    /// Whether a complete frame is waiting, so that acknowledgements can be sent per batch.
    fn frame_ready(&self) -> bool {
        !matches!(peek_server(self.pending()), Ok(Peek::Incomplete))
    }
}

/// The sending side of a connection.
enum Out {
    Quic(quinn::SendStream),
    Stream(Writer),
}

impl Out {
    async fn write_all(&mut self, bytes: &[u8]) -> bool {
        match self {
            Self::Quic(send) => send.write_all(bytes).await.is_ok(),
            Self::Stream(writer) => writer.write_all(bytes).await.is_ok(),
        }
    }

    /// Ends the stream after the data, and waits until the peer has it.
    async fn finish(&mut self) {
        match self {
            Self::Quic(send) => {
                let _ = send.finish();
                // the peer must have the last frame before the process ends.
                let _ = timeout(FLUSH_TIMEOUT, send.stopped()).await;
            }
            Self::Stream(writer) => {
                let _ = timeout(FLUSH_TIMEOUT, async {
                    let _ = writer.flush().await;
                    let _ = writer.shutdown().await;
                })
                .await;
            }
        }
    }
}

/// Sends frames from a channel, so that a stalled stream never blocks reading.
async fn write_frames(mut out: Out, mut frames: mpsc::UnboundedReceiver<ClientFrame>) {
    let mut bytes = Vec::new();
    while let Some(frame) = frames.recv().await {
        vlog!(3, "-> {}", client_frame_name(&frame));
        bytes.clear();
        if frame.encode(&mut bytes).is_err() {
            return;
        }
        // frames that are queued at the same moment go out in one write, so that a hop with
        // Nagle's algorithm does not hold the second of two small packets back for the delayed
        // ACK of the first.
        tokio::task::yield_now().await;
        while bytes.len() < 64 * 1024 {
            match frames.try_recv() {
                Ok(next) => {
                    vlog!(3, "-> {}", client_frame_name(&next));
                    if next.encode(&mut bytes).is_err() {
                        return;
                    }
                }
                Err(_) => break,
            }
        }
        if !out.write_all(&bytes).await {
            return;
        }
        // a stream with a buffer must not hold a keystroke back.
        if let Out::Stream(writer) = &mut out
            && writer.flush().await.is_err()
        {
            return;
        }
    }
    out.finish().await;
}

/// The next cause from the watch, or never if there is none.
async fn next_cause(net: &mut Option<mpsc::UnboundedReceiver<Cause>>) -> Option<Cause> {
    match net {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// The kind of a hello, for the debug output.
fn hello_name(hello: &Hello) -> &'static str {
    match hello {
        Hello::Start { .. } => "start",
        Hello::Resume { .. } => "resume",
        Hello::Attach { .. } => "attach",
    }
}

/// A client frame by kind and size, for the debug output. Never the content.
fn client_frame_name(frame: &ClientFrame) -> String {
    match frame {
        ClientFrame::Hello(hello) => hello_name(hello).to_owned(),
        ClientFrame::Input(data) => format!("input {} bytes", data.len()),
        ClientFrame::Compressed(data) => format!("compressed input {} bytes", data.len()),
        ClientFrame::Resize(size) => format!("resize {}x{}", size.cols(), size.rows()),
        ClientFrame::OutputAck(offset) => format!("output ack {offset}"),
        ClientFrame::Close => "close".to_owned(),
        ClientFrame::Detach => "detach".to_owned(),
        ClientFrame::Hangup => "hangup".to_owned(),
        ClientFrame::Ping => "ping".to_owned(),
        ClientFrame::AgentOffer => "agent offer".to_owned(),
        ClientFrame::Agent(id, data) => format!("agent {id} {} bytes", data.len()),
        ClientFrame::AgentClose(id) => format!("agent close {id}"),
    }
}

/// A server frame by kind and size, for the debug output.
fn server_frame_name(frame: &ServerFrame) -> String {
    match frame {
        ServerFrame::Welcome(w) => format!(
            "welcome, input offset {}, output offset {}",
            w.input_offset, w.output_offset
        ),
        ServerFrame::Output(data) => format!("output {} bytes", data.len()),
        ServerFrame::Compressed(data) => format!("compressed output {} bytes", data.len()),
        ServerFrame::InputAck(offset) => format!("input ack {offset}"),
        ServerFrame::Exit(status) => format!("exit {status:?}"),
        ServerFrame::Error(code) => format!("error {code:?}"),
        ServerFrame::Pong => "pong".to_owned(),
        ServerFrame::Gap(skipped) => format!("gap {skipped} bytes"),
        ServerFrame::Detached => "detached by `ssj detach`".to_owned(),
        ServerFrame::AgentOpen(id) => format!("agent open {id}"),
        ServerFrame::Agent(id, data) => format!("agent {id} {} bytes", data.len()),
        ServerFrame::AgentClose(id) => format!("agent close {id}"),
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
    /// The QUIC connection, if the link is one.
    quic: Option<quinn::Connection>,
    out: mpsc::UnboundedSender<ClientFrame>,
    writer: JoinHandle<()>,
    reader: FrameReader,
    last_heard: Instant,
    /// When the oldest ping that no pong has answered went out. Frames of the server that arrive
    /// meanwhile show that its direction works and say nothing about ours.
    ping_since: Option<Instant>,
    _guard: Option<Box<dyn Send + Sync>>,
}

impl Link {
    fn new(
        quic: Option<quinn::Connection>,
        out: Out,
        reader: Reader,
        guard: Option<Box<dyn Send + Sync>>,
    ) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            quic,
            out: tx,
            writer: tokio::spawn(write_frames(out, rx)),
            reader: FrameReader::new(reader),
            last_heard: Instant::now(),
            ping_since: None,
            _guard: guard,
        }
    }

    fn send(&self, frame: ClientFrame) {
        let _ = self.out.send(frame);
    }

    /// Sends a ping and remembers when, until the pong comes.
    fn ping(&mut self) {
        self.ping_since.get_or_insert_with(Instant::now);
        self.send(ClientFrame::Ping);
    }

    /// Lets the writer deliver what is queued, then ends the link.
    async fn finish(self) {
        drop(self.out);
        let _ = timeout(FLUSH_TIMEOUT * 2, self.writer).await;
    }
}

/// Makes the program in the session draw its screen again, by changing the size of the terminal
/// for a moment: debug lines that were written over the screen are then gone.
async fn redraw(size: WindowSize, out: &mpsc::UnboundedSender<ClientFrame>) {
    let narrower =
        WindowSize::new(size.rows(), size.cols().saturating_sub(1).max(1)).filter(|n| *n != size);
    if let Some(narrower) = narrower {
        let _ = out.send(ClientFrame::Resize(narrower));
        sleep(Duration::from_millis(60)).await;
    }
    let _ = out.send(ClientFrame::Resize(size));
}

enum Attempt {
    Linked(Link),
    /// Try again at once on the same transport.
    Again,
    /// Try again later.
    Retry,
    /// The session cannot continue.
    Fatal(String),
}

enum Pumped {
    Done(Finished),
    Lost,
    /// A better connection opened while this one worked: go on over it, as after a reconnect.
    Switch(Link, usize),
}

/// QUIC being tried on a new network, within a time limit.
type Upgrade =
    Pin<Box<dyn Future<Output = Result<Result<Link, String>, tokio::time::error::Elapsed>> + Send>>;

/// A connection that is being opened.
type LinkFuture = Pin<Box<dyn Future<Output = Result<Link, String>> + Send>>;

struct Client {
    /// The UDP socket, made when the first UDP attempt needs it.
    endpoint: Option<Endpoint>,
    params: Params,
    /// The tier that is tried now.
    current: usize,
    /// A connection that opened by itself and is to carry the session, with its tier.
    prepared: Option<(Link, usize)>,
    /// Failed attempts on the current tier since the last success.
    failures: usize,
    /// Whether a connection was ever made, so that the order of the tiers is settled.
    linked_once: bool,
    /// What the last failed attempt said, for the status message.
    note: Option<String>,
    io: Io,
    input: ReplayBuffer,
    /// Since when input is typed that the server has not acknowledged.
    input_since: Option<Instant>,
    output_offset: u64,
    /// Output that the server has been told about.
    acked_output: u64,
    echo: Echo,
    compress: bool,
    compressor: Compressor,
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
    /// Since when the status waits for the end of a sequence of the remote side.
    status_blocked_since: Option<Instant>,
}

/// Runs one session until it ends, and returns how it ended.
pub async fn run_session(params: Params, io: Io) -> Result<Finished, String> {
    if params.tiers.is_empty() {
        return Err("no way to reach the server".to_owned());
    }
    let size = io.size;
    let echo = Echo::new(params.local_echo, size.cols());
    let compress = params.compress;
    let mut io = io;
    let net = Some(std::mem::replace(&mut io.net, mpsc::unbounded_channel().1));
    Client {
        endpoint: None,
        params,
        current: 0,
        prepared: None,
        failures: 0,
        linked_once: false,
        note: None,
        io,
        net,
        tracker: Tracker::new(),
        status_wanted: None,
        status_shown: None,
        status_blocked_since: None,
        input: ReplayBuffer::new(INPUT_BUFFER),
        input_since: None,
        output_offset: 0,
        acked_output: 0,
        echo,
        compress,
        compressor: Compressor::new(),
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
                    agent: self.params.agent.is_some(),
                }
            };
            if attempts > 0 && attempts.is_multiple_of(RESOLVE_EVERY) {
                self.refresh_address().await;
            }
            vlog!(
                2,
                "attempt {}: {} over {}",
                attempts + 1,
                hello_name(&hello),
                self.params.tiers[self.current].name()
            );
            match self.establish_watched(&hello).await {
                Attempt::Fatal(message) => {
                    vlog!(1, "cannot continue: {message}");
                    self.clear_status_at_end().await;
                    return Err(message);
                }
                Attempt::Linked(mut link) => {
                    if !self.linked_once
                        && let Some(Announce {
                            destination,
                            name,
                            started,
                        }) = &self.params.announce
                    {
                        // without the message of the day of a login, nothing else says when the
                        // session begins. The terminal is raw by now, hence the carriage return.
                        let what = if *started || matches!(hello, Hello::Start { .. }) {
                            "started session"
                        } else {
                            "attached to session"
                        };
                        let id = self.params.connect.id.as_str();
                        let session = name
                            .as_ref()
                            .map_or_else(|| id.to_owned(), |name| format!("{name} ({id})"));
                        let line = format!(
                            "[{what} {session} on {destination} over {}]\r\n",
                            self.params.tiers[self.current].name()
                        );
                        let _ = self.io.output.write_all(line.as_bytes()).await;
                        let _ = self.io.output.flush().await;
                    }
                    attempts = 0;
                    self.failures = 0;
                    self.linked_once = true;
                    self.started = true;
                    vlog!(
                        1,
                        "session running over {}",
                        self.params.tiers[self.current].name()
                    );
                    self.clear_status().await;
                    let pumped = self.pump(&mut link).await;
                    // what was shown ahead of the server goes, whatever happens next.
                    let taken_back = self.echo.cancel();
                    let _ = self.io.output.write_all(&taken_back).await;
                    let _ = self.io.output.flush().await;
                    match pumped? {
                        Pumped::Done(finished) => {
                            vlog!(1, "session ended: {finished:?}");
                            link.finish().await;
                            self.clear_status_at_end().await;
                            return Ok(finished);
                        }
                        Pumped::Lost => vlog!(1, "connection lost, reconnecting"),
                        Pumped::Switch(better, tier) => {
                            vlog!(1, "switching to {}", self.params.tiers[tier].name());
                            self.prepared = Some((better, tier));
                            continue;
                        }
                    }
                }
                // the answer was fine and only the kind of hello was wrong: no pause, no other
                // tier.
                Attempt::Again => continue,
                Attempt::Retry => {
                    if self.try_next_tier() {
                        continue;
                    }
                }
            }
            attempts += 1;
            vlog!(
                1,
                "attempt {attempts} failed{}",
                self.note
                    .as_ref()
                    .map_or_else(String::new, |note| format!(": {note}"))
            );
            let text = match self.note.take() {
                Some(note) => format!("connection lost, reconnecting (attempt {attempts}, {note})"),
                None => format!("connection lost, reconnecting (attempt {attempts})"),
            };
            self.show_status(&text).await;
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
        if self.status_wanted == self.status_shown {
            self.status_blocked_since = None;
            return;
        }
        if !force && self.tracker.in_sequence() {
            self.status_blocked_since.get_or_insert_with(Instant::now);
            return;
        }
        self.status_blocked_since = None;
        let bytes = match &self.status_wanted {
            Some(text) => self.io.status.show(text),
            None => self.io.status.clear(self.tracker.title()),
        };
        let _ = self.io.output.write_all(&bytes).await;
        let _ = self.io.output.flush().await;
        self.status_shown = self.status_wanted.clone();
    }

    /// Lets a waiting status through when the remote side stopped in the middle of a sequence for
    /// good. The terminal is given a CAN, which ends the sequence in every terminal, and the rest
    /// of it is dropped when it arrives. A sequence that is merely slow is not touched.
    async fn unstick_status(&mut self) {
        let waited = self
            .status_blocked_since
            .is_some_and(|since| since.elapsed() >= SEQUENCE_PATIENCE);
        if waited && self.tracker.in_sequence() {
            let _ = self.io.output.write_all(b"\x18").await;
            let _ = self.io.output.flush().await;
            self.tracker.cancel_sequence();
            self.sync_status(false).await;
        }
    }

    /// Writes the notice for `dropped` forgotten bytes into the output.
    async fn note_gap(&mut self, dropped: u64) {
        let _ = self.io.output.write_all(&dropped_notice(dropped)).await;
        let _ = self.io.output.flush().await;
        // the rest of an open sequence is gone, so the tracker must not wait for it.
        self.tracker.abandon_sequence();
        self.sync_status(false).await;
    }

    /// Moves the UDP socket to the current network. QUIC then probes the new path at once, and
    /// the server follows the new address.
    fn rebind(&mut self) {
        let Some(endpoint) = &self.endpoint else {
            return;
        };
        let Some(addr) = self.params.tiers.iter().find_map(|t| match t {
            Tier::Udp { addr, .. } => Some(*addr),
            Tier::Stream { .. } => None,
        }) else {
            return;
        };
        match std::net::UdpSocket::bind(wildcard(&addr)) {
            Ok(socket) => {
                let local = socket.local_addr().ok();
                let result = endpoint.rebind(socket);
                vlog!(
                    1,
                    "moved the UDP socket to the current network (local {local:?}): {}",
                    result.map_or_else(|e| e.to_string(), |()| "ok".to_owned())
                );
            }
            Err(err) => vlog!(1, "cannot open a new UDP socket: {err}"),
        }
    }

    /// Looks up the address of the server again, since a move or a long sleep may have changed
    /// it. A failed lookup keeps the old address.
    async fn refresh_address(&mut self) {
        let Some(Tier::Udp {
            addr,
            resolver: Some(resolver),
        }) = self.params.tiers.get_mut(self.current)
        else {
            return;
        };
        let Ok(Ok(found)) = timeout(RESOLVE_TIMEOUT, resolver()).await else {
            return;
        };
        if found != *addr {
            vlog!(
                1,
                "the address of the server changed from {addr} to {found}"
            );
            let family_changed = found.is_ipv4() != addr.is_ipv4();
            *addr = found;
            if family_changed {
                // the socket must be of the same family as the address.
                self.rebind();
            }
        }
    }

    /// After a failed attempt: whether the next tier should be tried at once.
    ///
    /// Before the first connection every failure moves on, and a pause follows only after the last
    /// tier. After a connection was made, the tier that worked is kept for a few failures, since
    /// the network is the likelier culprit.
    fn try_next_tier(&mut self) -> bool {
        let count = self.params.tiers.len();
        if count < 2 {
            return false;
        }
        if !self.linked_once {
            self.current = (self.current + 1) % count;
            vlog!(
                1,
                "next transport: {}",
                self.params.tiers[self.current].name()
            );
            return self.current != 0;
        }
        self.failures += 1;
        if self.failures >= TIER_PATIENCE {
            self.failures = 0;
            self.current = (self.current + 1) % count;
            vlog!(
                1,
                "after {TIER_PATIENCE} failures, the next transport: {}",
                self.params.tiers[self.current].name()
            );
        }
        false
    }

    /// The UDP socket, made on first use.
    fn udp_endpoint(&mut self, addr: SocketAddr) -> Result<Endpoint, String> {
        if let Some(endpoint) = &self.endpoint {
            return Ok(endpoint.clone());
        }
        let mut endpoint = Endpoint::client(wildcard(&addr))
            .map_err(|e| format!("cannot open a UDP socket: {e}"))?;
        endpoint.set_default_client_config(quic_client(client_tls(
            &self.params.identity,
            self.params.connect.fingerprint,
        )));
        self.endpoint = Some(endpoint.clone());
        Ok(endpoint)
    }

    /// Starts opening the connection of tier `idx`. The returned future owns what it needs, so
    /// that several of them can run side by side.
    /// How long the connection has not worked in both directions: the longer of the silence of the
    /// server and the wait for an answer to a ping or to typed input.
    fn lag(&self, link: &Link) -> Duration {
        [link.ping_since, self.input_since]
            .into_iter()
            .flatten()
            .map(|since| since.elapsed())
            .max()
            .unwrap_or_default()
            .max(link.last_heard.elapsed())
    }

    fn prepare_link(&mut self, idx: usize) -> LinkFuture {
        let udp = match &self.params.tiers[idx] {
            Tier::Udp { addr, .. } => Some(*addr),
            Tier::Stream { .. } => None,
        };
        vlog!(
            1,
            "connecting over {}{}",
            self.params.tiers[idx].name(),
            udp.map_or_else(String::new, |addr| format!(" to {addr}"))
        );
        let opened = Instant::now();
        if let Some(addr) = udp {
            let connecting = self.udp_endpoint(addr).and_then(|endpoint| {
                endpoint
                    .connect(addr, SERVER_NAME)
                    .map_err(|e| format!("udp: {e}"))
            });
            return Box::pin(async move {
                let conn = connecting?.await.map_err(|e| format!("udp: {e}"))?;
                let (send, recv) = conn.open_bi().await.map_err(|e| format!("udp: {e}"))?;
                vlog!(
                    2,
                    "QUIC handshake done in {:?}, round trip {:?}, remote {}",
                    opened.elapsed(),
                    conn.rtt(),
                    conn.remote_address()
                );
                Ok(Link::new(Some(conn), Out::Quic(send), Box::new(recv), None))
            });
        }
        let Tier::Stream { name, dial } = &mut self.params.tiers[idx] else {
            unreachable!("not a UDP tier");
        };
        let name = *name;
        let dialing = dial();
        Box::pin(async move {
            let stream = dialing.await.map_err(|e| format!("{name}: {e}"))?;
            vlog!(2, "{name} stream open after {:?}", opened.elapsed());
            Ok(Link::new(
                None,
                Out::Stream(stream.writer),
                stream.reader,
                stream.guard,
            ))
        })
    }

    /// Opens a connection and returns it with the tier it belongs to.
    ///
    /// With QUIC first and a stream transport behind it, both are tried and the first connection
    /// that opens wins: QUIC gets a head start of [`RACE_STAGGER`] when it is the transport in use,
    /// and the stream starts at once when the session is on it already, so that a network where
    /// UDP does not work costs a fraction of a second and not the whole timeout of QUIC.
    async fn race(&mut self) -> Result<(Link, usize), String> {
        let racing =
            self.params.tiers.len() > 1 && matches!(self.params.tiers[0], Tier::Udp { .. });
        if !racing {
            let idx = self.current;
            return self.prepare_link(idx).await.map(|link| (link, idx));
        }
        let (primary, secondary) = (0, self.current.max(1));
        let stagger = if self.current > 0 {
            Duration::ZERO
        } else {
            RACE_STAGGER
        };
        // before the first connection QUIC is given up on quickly; later it may be slow to return.
        let udp_cap = if self.linked_once {
            self.params.attempt_timeout
        } else {
            self.params.first_attempt_timeout
        };
        let udp = self.prepare_link(primary);
        let mut first: Option<LinkFuture> = Some(Box::pin(async move {
            timeout(udp_cap, udp)
                .await
                .unwrap_or_else(|_| Err("udp: no answer".to_owned()))
        }));
        let mut second: Option<LinkFuture> = None;
        let mut second_started = false;
        let delay = sleep(stagger);
        tokio::pin!(delay);
        let mut errors: Vec<String> = Vec::new();
        loop {
            // the second one starts at once when the first is out.
            if first.is_none() && !second_started {
                second = Some(self.prepare_link(secondary));
                second_started = true;
            }
            if first.is_none() && second.is_none() {
                return Err(errors.join("; "));
            }
            tokio::select! {
                result = async { first.as_mut().expect("guarded").await }, if first.is_some() => {
                    match result {
                        Ok(link) => return Ok((link, primary)),
                        Err(error) => {
                            vlog!(1, "{error}");
                            errors.push(error);
                            first = None;
                        }
                    }
                }
                result = async { second.as_mut().expect("guarded").await }, if second.is_some() => {
                    match result {
                        Ok(link) => return Ok((link, secondary)),
                        Err(error) => {
                            vlog!(1, "{error}");
                            errors.push(error);
                            second = None;
                        }
                    }
                }
                () = &mut delay, if !second_started => {
                    second = Some(self.prepare_link(secondary));
                    second_started = true;
                }
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
                    Some(cause) => {
                        vlog!(1, "network change detected while connecting ({cause:?})");
                        self.rebind();
                    }
                    None => net = None,
                },
            }
        };
        self.net = net;
        attempt
    }

    /// Connects, sends the hello, and resends the input that the server has not consumed.
    async fn establish(&mut self, hello: &Hello) -> Attempt {
        let limit = self.params.attempt_timeout;
        let prepared = self.prepared.take();
        let attempt = timeout(limit, async {
            let (link, idx) = match prepared {
                Some(prepared) => prepared,
                None => self.race().await?,
            };
            vlog!(2, "sending {}", hello_name(hello));
            link.send(ClientFrame::Hello(hello.clone()));
            let mut link = link;
            let reply = link.reader.next().await;
            Ok::<_, String>((link, idx, reply))
        })
        .await;
        let (link, reply) = match attempt {
            Ok(Ok((link, idx, reply))) => {
                self.current = idx;
                (link, reply)
            }
            Ok(Err(note)) => {
                vlog!(1, "connecting failed: {note}");
                self.note = Some(sanitize(note.as_bytes(), NOTE_CHARS));
                return Attempt::Retry;
            }
            Err(_) => {
                vlog!(1, "no answer within {limit:?}");
                self.note = Some(format!(
                    "{}: no answer",
                    self.params.tiers[self.current].name()
                ));
                return Attempt::Retry;
            }
        };
        let welcome = match reply {
            Ok(Some(ServerFrame::Welcome(welcome))) => welcome,
            // the first start may not have arrived, so it is tried again as a start
            // nobody has started the shell yet, so this client does it.
            Ok(Some(ServerFrame::Error(ErrorCode::NotStarted)))
                if matches!(hello, Hello::Attach { .. }) && !self.started =>
            {
                vlog!(1, "no running session to join: starting a new one");
                self.params.attach = false;
                return Attempt::Again;
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
                return Attempt::Again;
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
            // a client without history takes over the offsets of the server.
            self.output_offset = welcome.output_offset;
            self.acked_output = welcome.output_offset;
            self.input = ReplayBuffer::starting_at(INPUT_BUFFER, welcome.input_offset);
            if welcome.output_offset > 0 {
                // the replay starts in the middle of the stream: cancel a sequence it may cut
                // and reset the colors, so that the first line does not inherit a state.
                let _ = self.io.output.write_all(b"\x18\x1b[0m").await;
                let _ = self.io.output.flush().await;
            }
        }
        vlog!(
            2,
            "welcome: the server has {} bytes of our input and sends output from {}",
            welcome.input_offset,
            welcome.output_offset
        );
        match hello {
            Hello::Attach { .. } => {
                vlog!(
                    1,
                    "joined the running session {}",
                    self.params.connect.id.as_str()
                );
                vlog!(
                    2,
                    "the server replays its output from offset {}",
                    welcome.output_offset
                );
            }
            Hello::Resume { output_offset, .. } => {
                vlog!(1, "resumed the session {}", self.params.connect.id.as_str());
                vlog!(2, "resumed at output offset {output_offset}");
            }
            Hello::Start { .. } => {
                vlog!(1, "started the session {}", self.params.connect.id.as_str())
            }
        }
        if check_welcome(hello, welcome, self.input.start(), self.input.end()).is_err() {
            return Attempt::Fatal("the server answered with unexpected offsets".to_owned());
        }
        if self.input.ack(welcome.input_offset).is_err() {
            return Attempt::Fatal("the server consumed input that was never sent".to_owned());
        }
        // the server forgot output while the client was away. The stream goes on at the new
        // offset, and the gap is shown in the output where it happened, with its exact size.
        if let Hello::Resume { output_offset, .. } = hello
            && welcome.output_offset > *output_offset
        {
            vlog!(
                1,
                "the server forgot {} bytes of output while the client was away",
                welcome.output_offset - output_offset
            );
            self.output_offset = welcome.output_offset;
            self.note_gap(welcome.output_offset - output_offset).await;
        }
        // everything after the offset of the server is sent again, including what was typed
        // offline.
        let mut offset = welcome.input_offset;
        while let Ok(chunk) = self.input.read_from(offset, MAX_DATA_LEN) {
            if chunk.is_empty() {
                break;
            }
            offset += chunk.len() as u64;
            vlog!(2, "sending {} bytes of input again", chunk.len());
            link.send(ClientFrame::Input(chunk));
        }
        self.input_since = (self.input.end() > self.input.start()).then(Instant::now);
        if self.params.agent.is_some() {
            link.send(ClientFrame::AgentOffer);
        }
        Attempt::Linked(link)
    }

    /// Takes typed input into the buffer while there is no connection.
    ///
    /// Returns when the pause is over, or earlier if the user leaves with the escape.
    async fn wait_offline(&mut self, pause: Duration) -> Result<Option<Finished>, String> {
        let pause = sleep(pause);
        tokio::pin!(pause);
        let mut tick = interval(PING_EVERY);
        loop {
            let room = self.input.free().saturating_sub(1).min(self.chunk.len());
            tokio::select! {
                _ = &mut pause => return Ok(None),
                _ = tick.tick() => self.unstick_status().await,
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
                Some(()) = self.io.hangups.recv() => {
                    vlog!(1, "told to end while the connection is down: the session keeps running");
                    return Ok(Some(Finished::HungUp));
                }
                cause = next_cause(&mut self.net) => match cause {
                    // the network is different now, so waiting longer makes no sense.
                    Some(_) => {
                        self.rebind();
                        return Ok(None);
                    }
                    None => self.net = None,
                },
            }
        }
    }

    /// The frame for typed bytes: compressed when the uplink is what holds the input back (a
    /// paste over a slow link), plain otherwise. Typing is a few bytes and never compressed.
    /// Tells the server how much output was shown: at once when it is a lot, or when `force` says
    /// that a frame goes out anyway, and otherwise not yet.
    ///
    /// An acknowledgement alone is a small packet that nobody answers. On the TCP transport the
    /// next small packet behind it, such as a keystroke, can wait for the acknowledgement timer of
    /// the other side, which runs for 40 to 100 ms (see `bench/wprobe.py`). Sent in the write of
    /// the keystroke, it costs nothing.
    fn ack_output(&mut self, out: &mpsc::UnboundedSender<ClientFrame>, force: bool) {
        let unacked = self.output_offset.saturating_sub(self.acked_output);
        if unacked > 0 && (force || unacked >= ACK_BYTES) {
            let _ = out.send(ClientFrame::OutputAck(self.output_offset));
            self.acked_output = self.output_offset;
        }
    }

    fn input_frame(&mut self, bytes: &[u8]) -> ClientFrame {
        let waiting = self.input.end() - self.input.start();
        if self.compress
            && waiting >= COMPRESS_BACKLOG
            && bytes.len() >= COMPRESS_MIN_CHUNK
            && let Some(packed) = self.compressor.compress(bytes)
        {
            vlog!(
                2,
                "compressed {} bytes of input to {}",
                bytes.len(),
                packed.len()
            );
            return ClientFrame::Compressed(packed);
        }
        ClientFrame::Input(bytes.to_vec())
    }

    /// Applies typed bytes: input for the shell, up to the detach key, which leaves the session.
    async fn handle_typed(
        &mut self,
        typed: &[u8],
        link: Option<&mpsc::UnboundedSender<ClientFrame>>,
    ) -> Result<Option<Finished>, String> {
        let key_at = self
            .params
            .detach_key
            .and_then(|key| typed.iter().position(|byte| *byte == key));
        let bytes = &typed[..key_at.unwrap_or(typed.len())];
        if !bytes.is_empty() {
            let stored = self.input.push(bytes);
            self.input_since.get_or_insert_with(Instant::now);
            if stored < bytes.len() {
                // the reads leave room for what is free, so this does not happen.
                return Err("the input buffer overflowed".to_owned());
            }
            if let Some(link) = link {
                // the acknowledgement of the output goes out in the same write.
                self.ack_output(link, true);
                let frame = self.input_frame(bytes);
                let _ = link.send(frame);
                // only with a connection: without one the echo cannot come.
                let drawn = self.echo.typed(bytes, Instant::now());
                if !drawn.is_empty() {
                    let _ = self.io.output.write_all(&drawn).await;
                    let _ = self.io.output.flush().await;
                }
            }
        }
        if key_at.is_none() {
            return Ok(None);
        }
        if let Some(link) = link {
            let _ = link.send(ClientFrame::Detach);
        }
        Ok(Some(Finished::Detached))
    }

    /// Passes bytes between the terminal and the server until the session ends or the link breaks.
    async fn pump(&mut self, link: &mut Link) -> Result<Pumped, String> {
        let mut ping = interval(PING_EVERY);
        let mut stalled = false;
        // a change of the network that is waiting for the notifications to stop.
        let mut change: Option<(Cause, tokio::time::Instant)> = None;
        // when the server has to have answered after the client moved to a new path.
        let mut probe: Option<tokio::time::Instant> = None;
        // when the probe went out, and how many notices the change brought, for the debug output.
        let mut probed_at: Option<Instant> = None;
        let mut notices = 0usize;
        let mut resizes = Debounce::new(RESIZE_QUIET);
        // QUIC tried again on a new network while the session runs over a stream.
        let mut upgrade: Option<Upgrade> = None;
        // a second connection over the same stream transport, opened while the stream is silent.
        let mut spares: tokio::task::JoinSet<
            Result<Result<Link, String>, tokio::time::error::Elapsed>,
        > = tokio::task::JoinSet::new();
        let mut spare_not_before = Instant::now();
        // when to make the program draw again, after debug lines were written over the screen.
        let mut redraw_at: Option<tokio::time::Instant> = None;
        // the channels to the agent live as long as the link.
        let mut agent = self
            .params
            .agent
            .clone()
            .map(|socket| Forward::new(socket, link.out.clone()));
        loop {
            let room = self.input.free().min(self.chunk.len());
            let change_at = change.map(|(_, at)| at);
            if crate::log::take_terminal_lines() {
                redraw_at = Some(tokio::time::Instant::now() + REDRAW_AFTER_LOG);
            }
            let echo_due = self.echo.due();
            tokio::select! {
                read = self.io.input.read(&mut self.chunk[..room]), if self.stdin_open && room > 0 => {
                    match read {
                        Ok(n) if n > 0 => {
                            let typed = self.chunk[..n].to_vec();
                            if let Some(finished) = self.handle_typed(&typed, Some(&link.out)).await? {
                                return Ok(Pumped::Done(finished));
                            }
                        }
                        // end of input: the shell keeps running, like with `ssh`.
                        _ => self.stdin_open = false,
                    }
                }
                frame = link.reader.next() => {
                    let Some(frame) = frame? else {
                        vlog!(1, "the server closed the connection");
                        return Ok(Pumped::Lost);
                    };
                    link.last_heard = Instant::now();
                    if probe.take().is_some()
                        && let Some(sent) = probed_at.take()
                    {
                        vlog!(1, "the server answered on the new network after {:?}: the connection stays", sent.elapsed());
                    }
                    if std::mem::take(&mut stalled) {
                        self.clear_status().await;
                    }
                    vlog!(3, "<- {}", server_frame_name(&frame));
                    // compressed output is the same as plain output once it is inflated.
                    let frame = match frame {
                        ServerFrame::Compressed(packed) => ServerFrame::Output(
                            inflate(&packed)
                                .map_err(|e| format!("the server sent bad compressed output: {e:?}"))?,
                        ),
                        other => other,
                    };
                    match frame {
                        ServerFrame::Output(mut data) => {
                            // frames that are here already go to the terminal in one write. A
                            // terminal hands out its output in small pieces, and one write (a
                            // trip through the blocking pool) for each would cap the throughput.
                            while data.len() < OUTPUT_BATCH && link.reader.take_ready_output(&mut data) {}
                            // the rest of a sequence that was cancelled on the terminal is dropped.
                            let skipped = self.tracker.skip_prefix(&data);
                            let shown = &data[skipped..];
                            let (before, after) = self.echo.output(shown, Instant::now());
                            let written = async {
                                self.io.output.write_all(&before).await?;
                                self.io.output.write_all(shown).await?;
                                self.io.output.write_all(&after).await?;
                                self.io.output.flush().await
                            };
                            if written.await.is_err() {
                                return Err("cannot write to the terminal".to_owned());
                            }
                            self.tracker.feed(shown);
                            // a status that was held back because of a sequence can go out now.
                            self.sync_status(false).await;
                            self.output_offset += data.len() as u64;
                            if !link.reader.frame_ready() {
                                self.ack_output(&link.out, false);
                            }
                        }
                        ServerFrame::InputAck(offset) => {
                            self.input
                                .ack(offset)
                                .map_err(|_| "the server acknowledged input that was never sent".to_owned())?;
                            self.input_since = (self.input.end() > self.input.start()).then(Instant::now);
                        }
                        ServerFrame::Pong => link.ping_since = None,
                        ServerFrame::AgentOpen(id) => match agent.as_mut() {
                            Some(agent) => agent.open(id).await,
                            // nothing was offered, so a server that opens a channel is wrong.
                            None => link.send(ClientFrame::AgentClose(id)),
                        },
                        ServerFrame::Agent(id, data) => {
                            if let Some(agent) = agent.as_mut() {
                                agent.data(id, &data);
                            }
                        }
                        ServerFrame::AgentClose(id) => {
                            if let Some(agent) = agent.as_mut() {
                                agent.close(id);
                            }
                        }
                        ServerFrame::Gap(skipped) => {
                            // the server skipped output that this client was too slow to take.
                            // What was shown ahead of the server goes first, then the notice.
                            vlog!(1, "the server skipped {skipped} bytes of output (slow link)");
                            let taken_back = self.echo.cancel();
                            let _ = self.io.output.write_all(&taken_back).await;
                            self.output_offset = self
                                .output_offset
                                .checked_add(skipped)
                                .ok_or("the server skipped more output than there can be")?;
                            self.note_gap(skipped).await;
                            self.ack_output(&link.out, true);
                        }
                        ServerFrame::Detached => {
                            vlog!(1, "`ssj detach` was run for this session: leaving it");
                            return Ok(Pumped::Done(Finished::Detached));
                        }
                        ServerFrame::Exit(status) => {
                            link.send(ClientFrame::Close);
                            return Ok(Pumped::Done(Finished::Exited(exit_code(status))));
                        }
                        ServerFrame::Error(code) => return Err(format!("the server ended the session: {code:?}")),
                        ServerFrame::Welcome(_) => return Err("the server sent a second welcome".to_owned()),
                        // turned into output above.
                        ServerFrame::Compressed(_) => unreachable!("inflated above"),
                    }
                }
                Some(()) = self.io.hangups.recv() => {
                    if self.params.hangup_detaches {
                        // the connection of an `ssh` session ended, which is a loss and not a wish.
                        vlog!(1, "the terminal was closed inside an ssh session: leaving the session running");
                        link.send(ClientFrame::Detach);
                        return Ok(Pumped::Done(Finished::Detached));
                    }
                    vlog!(1, "the terminal was closed or ssj was told to end: ending the session");
                    link.send(ClientFrame::Hangup);
                    return Ok(Pumped::Done(Finished::HungUp));
                }
                Some(size) = self.io.resizes.recv() => {
                    vlog!(2, "the terminal is now {}x{}", size.cols(), size.rows());
                    self.size = size;
                    self.echo.resize(size.cols());
                    resizes.push(size, tokio::time::Instant::now());
                }
                _ = async { tokio::time::sleep_until(tokio::time::Instant::from_std(echo_due.expect("guarded"))).await }, if echo_due.is_some() => {
                    // nobody echoed what was shown, as with a password prompt.
                    let taken_back = self.echo.expire(Instant::now());
                    let _ = self.io.output.write_all(&taken_back).await;
                    let _ = self.io.output.flush().await;
                }
                _ = async { tokio::time::sleep_until(resizes.due().expect("guarded")).await }, if resizes.due().is_some() => {
                    if let Some(size) = resizes.take(tokio::time::Instant::now()) {
                        link.send(ClientFrame::Resize(size));
                    }
                }
                cause = next_cause(&mut self.net) => match cause {
                    Some(cause) => {
                        notices += 1;
                        vlog!(2, "network change notice {notices}: {cause:?}");
                        let strongest = change.map_or(cause, |(old, _)| old.max(cause));
                        change = Some((strongest, tokio::time::Instant::now() + CHANGE_QUIET));
                    }
                    None => self.net = None,
                },
                _ = async { tokio::time::sleep_until(change_at.expect("guarded")).await }, if change_at.is_some() => {
                    let (cause, _) = change.take().expect("guarded");
                    let transport = self.params.tiers[self.current].name();
                    vlog!(
                        1,
                        "network change ({cause:?}, {notices} notices): {}",
                        if link.quic.is_some() {
                            "moving the UDP socket and probing the server".to_owned()
                        } else {
                            format!("the {transport} stream has no socket to move, probing the server")
                        }
                    );
                    notices = 0;
                    self.rebind();
                    // a faster network may have appeared: QUIC is tried on it while the stream
                    // works.
                    if self.current != 0
                        && upgrade.is_none()
                        && matches!(self.params.tiers[0], Tier::Udp { .. })
                    {
                        vlog!(1, "trying QUIC on the new network");
                        let attempt = self.prepare_link(0);
                        upgrade = Some(Box::pin(timeout(UPGRADE_TIMEOUT, attempt)));
                    }
                    link.ping();
                    probed_at = Some(Instant::now());
                    let wait = if cause == Cause::Woke { PROBE_AFTER_WAKE } else { PROBE_AFTER_CHANGE };
                    probe = Some(tokio::time::Instant::now() + wait);
                }
                _ = async { tokio::time::sleep_until(probe.expect("guarded")).await }, if probe.is_some() => {
                    // the new path did not bring an answer, so the old connection is not worth
                    // keeping.
                    vlog!(1, "no answer after the network change: giving up the connection and reconnecting");
                    return Ok(Pumped::Lost);
                }
                result = async { upgrade.as_mut().expect("guarded").await }, if upgrade.is_some() => {
                    upgrade = None;
                    match result {
                        Ok(Ok(better)) => {
                            vlog!(1, "QUIC works on the new network");
                            return Ok(Pumped::Switch(better, 0));
                        }
                        Ok(Err(error)) => vlog!(1, "QUIC does not work on the new network: {error}"),
                        Err(_) => vlog!(
                            1,
                            "QUIC has no answer on the new network: staying on {}",
                            self.params.tiers[self.current].name()
                        ),
                    }
                }
                Some(joined) = spares.join_next(), if !spares.is_empty() => {
                    match joined {
                        Ok(Ok(Ok(better))) => {
                            // the stream may have come back while the spare was opening, and then
                            // it is the better one: nothing is in flight on it that a move
                            // would have to repeat.
                            if self.lag(link) < SPARE_AFTER / 2 {
                                vlog!(1, "a spare connection is open, but the stream answers again: keeping the stream");
                                spares.abort_all();
                            } else {
                                vlog!(1, "a spare connection is open: moving to it");
                                return Ok(Pumped::Switch(better, self.current));
                            }
                        }
                        Ok(Ok(Err(error))) => {
                            vlog!(1, "a spare connection failed: {error}");
                            spare_not_before = Instant::now() + SPARE_PAUSE;
                        }
                        Ok(Err(_)) => vlog!(1, "a spare connection did not open in time"),
                        Err(_) => {}
                    }
                }
                () = async { tokio::time::sleep_until(redraw_at.expect("guarded")).await }, if redraw_at.is_some() => {
                    redraw_at = None;
                    redraw(self.size, &link.out).await;
                }
                _ = ping.tick() => {
                    // A frame that arrived a moment ago shows that the connection works, so a ping
                    // is for the quiet times. It also takes the acknowledgement of the output with
                    // it, so that none goes out alone.
                    if link.last_heard.elapsed() >= PING_EVERY / 2 {
                        self.ack_output(&link.out, true);
                        link.ping();
                    }
                    self.unstick_status().await;
                    let silent = link.last_heard.elapsed();
                    // the server may be heard while it hears nothing: a ping or typed input
                    // without an answer is as bad as silence.
                    let lag = self.lag(link);
                    let stall_after = link
                        .quic
                        .as_ref()
                        .map_or(STALL_MIN, |conn| (conn.rtt() * 4).clamp(STALL_MIN, STALL_MAX));
                    // QUIC finds a returning network by itself and survives long outages. A
                    // stream has no way to notice that its path is gone, and the `ssh` behind it
                    // would hang, so a long silence ends it and a new one is made.
                    // not while a spare is opening, which would lose its progress, but not for ever:
                    // a transport that never opens has to give way to the next one.
                    let patience = if !spares.is_empty() {
                        self.params.stream_dead_after * 3
                    } else {
                        self.params.stream_dead_after
                    };
                    if link.quic.is_none() && lag > patience {
                        vlog!(1, "no answer for {lag:?} on a stream: giving it up");
                        return Ok(Pumped::Lost);
                    }
                    if link.quic.is_none() {
                        if lag > SPARE_AFTER
                            && spares.len() < SPARES_AT_ONCE
                            && upgrade.is_none()
                            && Instant::now() >= spare_not_before
                        {
                            vlog!(
                                1,
                                "no answer for {lag:?} on a stream: opening a spare connection ({} open)",
                                spares.len()
                            );
                            let attempt = self.prepare_link(self.current);
                            spares.spawn(timeout(SPARE_TIMEOUT, attempt));
                        } else if lag < SPARE_AFTER / 2 && !spares.is_empty() {
                            vlog!(1, "the stream answers again: dropping the spare connections");
                            spares.abort_all();
                            while spares.try_join_next().is_some() {}
                        }
                    }
                    if silent > stall_after {
                        stalled = true;
                        // the number changes in steps of five seconds, so the title does not
                        // flicker.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn vars() -> Vec<(String, String)> {
        [
            ("TERM", "xterm-ghostty"),
            ("LC_THEME", "dark"),
            ("HOME", "/h"),
        ]
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .to_vec()
    }

    fn names(filter: Vec<String>) -> Vec<String> {
        let env = selected_env(vars(), &EnvFilter::new(filter).unwrap()).unwrap();
        env.iter().map(|(name, _)| name.to_owned()).collect()
    }

    #[test]
    fn term_is_sent_even_when_the_user_names_other_variables() {
        assert_eq!(names(vec!["LC_THEME".to_owned()]), ["TERM", "LC_THEME"]);
        assert_eq!(names(vec![]), ["TERM"]);
    }

    fn texts(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn what_ssh_sends_is_sent_and_without_an_answer_the_default_is() {
        let none = EnvFilter::none();
        let ssh = send_patterns(&none, &texts(&["A"]), Some(texts(&["LC_THEME"]))).unwrap();
        assert!(ssh.matches("LC_THEME") && ssh.matches("A"));
        // ssh sends no locale here, and neither does ssj.
        assert!(!ssh.matches("LANG") && !ssh.matches("LC_ALL"));
        let nothing = send_patterns(&none, &[], Some(Vec::new())).unwrap();
        assert!(!nothing.matches("LANG"));
        let fallback = send_patterns(&none, &texts(&["A"]), None).unwrap();
        assert!(fallback.matches("LANG") && fallback.matches("LC_ALL") && fallback.matches("A"));
    }

    #[test]
    fn the_config_file_adds_to_ssh() {
        let config = EnvFilter::new(texts(&["EDITOR"])).unwrap();
        let both = send_patterns(&config, &texts(&["A"]), Some(texts(&["LANG"]))).unwrap();
        assert!(both.matches("EDITOR") && both.matches("A") && both.matches("LANG"));
    }

    #[test]
    fn the_default_selection_does_not_send_a_variable_twice() {
        let env = selected_env(vars(), &EnvFilter::default_patterns()).unwrap();
        assert_eq!(env.len(), 2);
    }
}
