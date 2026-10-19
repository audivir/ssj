//! The session of the client: connect, pass bytes between terminal and server, and reconnect.

use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::pin::Pin;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use quinn::Endpoint;
use ssj_net::{Identity, SERVER_NAME, client_tls, quic_client};
use ssj_proto::bootstrap::{Connect, Prestart, sanitize};
use ssj_proto::compress::{Compressor, inflate};
use ssj_proto::control::{Reply, SessionInfo, SessionState, parse_reply};
use ssj_proto::frame::MAX_DATA_LEN;
use ssj_proto::resume::check_welcome;
use ssj_proto::{
    ClientFrame, Env, ErrorCode, ExitStatus, Hello, Peek, ReplayBuffer, ServerFrame, WindowSize,
    decode_server, peek_server,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{interval, sleep, timeout};

use crate::agent::Forward;
use crate::bootstrap::{StartError, run_remote, start_server};
use crate::cli::{Args, Command, LocalEcho, Transport};
use crate::config::Config;
use crate::debounce::Debounce;
use crate::echo::Echo;
use crate::escape::{Action, EscapeFilter, HELP};
use crate::install::{Decision, confirm, decide, install, interactive};
use crate::netwatch::Cause;
use crate::sessions_cache;
use crate::status::{LineStatus, Status, TitleStatus, dropped_notice};
use crate::terminal::{RawMode, window_size};
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
/// Failed attempts on a tier that worked before, until the next tier is tried.
const TIER_PATIENCE: usize = 3;
/// Time the first tier gets in the first connection when other tiers follow.
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
    /// The terminal was closed or `ssj` was told to end, and the server was asked to end the
    /// session (it could not be, if the connection was down).
    HungUp,
}

/// Looks up the address of the server again.
pub type Resolver =
    Box<dyn FnMut() -> Pin<Box<dyn Future<Output = Result<SocketAddr, String>> + Send>> + Send>;

pub type Reader = Box<dyn AsyncRead + Send + Unpin>;
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
    /// `None` turns the escape character off.
    pub escape: Option<u8>,
    /// Join a running session instead of starting one.
    pub attach: bool,
    /// Show typed characters before the server echoes them (see [`crate::echo`]).
    pub local_echo: bool,
    /// Compress input while the link is the limit (the server decides for its output).
    pub compress: bool,
    /// The socket of the local ssh agent, to offer it to the session.
    pub agent: Option<PathBuf>,
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
    // the runtime to finish would hang forever
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

/// Selected variables of the local environment.
fn local_env(args: &Args) -> Result<Env, String> {
    let vars = std::env::vars_os()
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
        .filter(|(name, _)| args.send_env.matches(name))
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
    if let Some(choice) = args.congestion {
        ssj_net::set_congestion(choice);
        args.server_args
            .extend(["--congestion".to_owned(), choice.as_str().to_owned()]);
    }
    if args.batch {
        // nobody can answer a prompt of a background refresh, and nobody waits for it
        args.ssh_command
            .extend(["-o", "BatchMode=yes", "-o", "ConnectTimeout=2"].map(str::to_owned));
    }
    match args.command.clone() {
        Command::Complete(host) => complete(host.as_deref()),
        Command::Connect => {
            // a name or `auto_attach` may mean a session that runs already; the server decides,
            // and the client starts with an attach, which a new session answers with `NotStarted`
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
    prestart: Option<&Prestart>,
) -> Result<Connect, String> {
    vlog!(
        1,
        "starting the server on {} through {}",
        args.destination,
        args.ssh_command[0]
    );
    match start_server(args, fingerprint, extra, prestart).await {
        Ok(connect) => Ok(connect),
        Err(StartError::Failed(message)) => Err(message),
        Err(StartError::Missing) if attach => Err(format!(
            "ssj-server is not installed on {}, so no session runs there",
            args.destination
        )),
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

/// The sessions as a JSON array, one object each.
pub fn sessions_json(infos: &[SessionInfo]) -> String {
    // the id and the name are restricted to characters that need no escaping
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
fn complete(host: Option<&str>) -> u8 {
    let Some(dir) = sessions_cache::dir() else {
        return 0;
    };
    for name in sessions_cache::names(&dir, host) {
        println!("{name}");
    }
    let Ok(exe) = std::env::current_exe() else {
        return 0;
    };
    for origin in sessions_cache::stale_origins(&dir, sessions_cache::REFRESH_AFTER) {
        let target = match origin.port {
            Some(port) if origin.destination.contains(':') => {
                // an IPv6 address needs brackets in front of a port
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
        // the list writes the cache; the process is not waited for
        let _ = std::process::Command::new(&exe)
            .args(["--batch", "list", &target])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
    0
}

fn remember(origin: &sessions_cache::Origin, names: &[String]) {
    if let Some(dir) = sessions_cache::dir() {
        // a cache that cannot be written only costs the completion
        let _ = sessions_cache::store(&dir, origin, names);
    }
}

async fn list_sessions(args: &Args) -> u8 {
    // one refresh per host at a time; a second one has nothing to add
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
        names.extend(info.name.as_ref().map(|name| name.as_str().to_owned()));
        names.push(info.id.as_str().to_owned());
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
    for info in infos {
        println!(
            "{}  {:<12}  {:>4}  protocol {}  {}",
            info.id.as_str(),
            state_word(info.state),
            format_age(info.age_secs),
            info.protocol,
            info.name.as_ref().map_or("-", |name| name.as_str())
        );
    }
    0
}

async fn kill_session(args: &Args, id: Option<String>) -> u8 {
    let mut extra = vec!["--kill".to_owned()];
    extra.extend(id);
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

/// The ways to reach the server, in the order to try them, from the choice of the user and what
/// the server offers.
/// What the client finds out while the `ssh` of the bootstrap is still busy, so that the start
/// does not wait for it afterwards.
#[derive(Default)]
struct Prefetched {
    config: Option<SshConfig>,
    /// The address for UDP, without the port.
    address: Option<IpAddr>,
}

/// Asks `ssh -G` and looks up the address that UDP will use.
async fn prefetch(args: &Args) -> Prefetched {
    if !matches!(args.transport, Transport::Auto | Transport::Udp) {
        return Prefetched::default();
    }
    let config = crate::tunnel::ssh_config(args).await;
    vlog!(
        2,
        "ssh -G: HostName {:?}, jump host or proxy: {}",
        config.as_ref().and_then(|c| c.hostname.as_deref()),
        config.as_ref().is_some_and(|c| c.jump)
    );
    let host = udp_host(args, config.as_ref());
    let address = resolve(&host, 0).await.ok().map(|addr| addr.ip());
    vlog!(2, "the address of {host} for UDP: {address:?}");
    Prefetched { config, address }
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
    prefetched: Prefetched,
) -> Result<Vec<Tier>, String> {
    let choice = args.transport;
    // UDP does not travel through a jump host, so it is not even tried there
    let udp_possible = matches!(choice, Transport::Auto | Transport::Udp) && connect.udp.is_some();
    let config = prefetched.config.filter(|_| udp_possible);
    let jump = choice == Transport::Auto && config.as_ref().is_some_and(|c| c.jump);
    let order =
        crate::tunnel::tier_order(choice, jump, connect.udp.is_some(), connect.tcp.is_some());
    let mut tiers = Vec::new();
    let mut notes = Vec::new();
    for kind in order {
        match kind {
            Kind::Udp => {
                let port = connect.udp.expect("offered");
                // the address of the server: the one given, else the `HostName` of the `ssh`
                // configuration (the destination may be an alias), else the destination itself
                let host = udp_host(args, config.as_ref());
                let found = match prefetched.address {
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
        let env = local_env(&args)?;
        let agent = agent_socket(&args)?;
        let prestart = fresh.then(|| Prestart {
            size: window_size(),
            env: env.clone(),
            agent: agent.is_some(),
        });
        let (connect, prefetched) = tokio::join!(
            start_or_install(
                &args,
                config,
                &fingerprint,
                extra,
                attach,
                prestart.as_ref()
            ),
            prefetch(&args)
        );
        let connect = connect?;
        vlog!(
            1,
            "the server runs: session {}, udp {:?}, tcp {:?}",
            connect.id.as_str(),
            connect.udp,
            connect.tcp
        );
        let tiers = build_tiers(&args, &identity, &connect, prefetched).await?;
        vlog!(
            1,
            "transports in order: {}",
            tiers.iter().map(Tier::name).collect::<Vec<_>>().join(", ")
        );
        Ok::<_, String>((connect, env, tiers, agent))
    }
    .await;
    let (connect, env, tiers, agent) = match prepared {
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
            escape: args.escape,
            attach,
            local_echo: raw.is_some() && args.local_echo == LocalEcho::Auto,
            compress: args.compress == Some(true),
            agent,
        },
        io,
    )
    .await;
    drop(raw);

    match result {
        Ok(Finished::Exited(code)) => code,
        Ok(Finished::HungUp) => 129,
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
            // Read straight into the spare room of the buffer. This future is dropped when
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
                // the peer must have the last frame before the process ends
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
        if frame.encode(&mut bytes).is_err() || !out.write_all(&bytes).await {
            return;
        }
        // a stream with a buffer must not hold a keystroke back
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
            _guard: guard,
        }
    }

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
}

struct Client {
    /// The UDP socket, made when the first UDP attempt needs it.
    endpoint: Option<Endpoint>,
    params: Params,
    /// The tier that is tried now.
    current: usize,
    /// Failed attempts on the current tier since the last success.
    failures: usize,
    /// Whether a connection was ever made, so that the order of the tiers is settled.
    linked_once: bool,
    /// What the last failed attempt said, for the status message.
    note: Option<String>,
    io: Io,
    input: ReplayBuffer,
    output_offset: u64,
    escape: EscapeFilter,
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

pub async fn run_session(params: Params, io: Io) -> Result<Finished, String> {
    if params.tiers.is_empty() {
        return Err("no way to reach the server".to_owned());
    }
    let size = io.size;
    let escape = EscapeFilter::new(params.escape);
    let echo = Echo::new(params.local_echo, size.cols());
    let compress = params.compress;
    let mut io = io;
    let net = Some(std::mem::replace(&mut io.net, mpsc::unbounded_channel().1));
    Client {
        endpoint: None,
        params,
        current: 0,
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
        output_offset: 0,
        escape,
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
                    // what was shown ahead of the server goes, whatever happens next
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
                    }
                }
                // the answer was fine and only the kind of hello was wrong: no pause, no other tier
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
        // the rest of an open sequence is gone, so the tracker must not wait for it
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
                // the socket must be of the same family as the address
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

    /// Opens the connection of the current tier.
    async fn open_link(&mut self) -> Result<Link, String> {
        let udp = match &self.params.tiers[self.current] {
            Tier::Udp { addr, .. } => Some(*addr),
            Tier::Stream { .. } => None,
        };
        vlog!(
            1,
            "connecting over {}{}",
            self.params.tiers[self.current].name(),
            udp.map_or_else(String::new, |addr| format!(" to {addr}"))
        );
        let opened = Instant::now();
        if let Some(addr) = udp {
            let endpoint = self.udp_endpoint(addr)?;
            let connecting = endpoint
                .connect(addr, SERVER_NAME)
                .map_err(|e| format!("udp: {e}"))?;
            let conn = connecting.await.map_err(|e| format!("udp: {e}"))?;
            let (send, recv) = conn.open_bi().await.map_err(|e| format!("udp: {e}"))?;
            vlog!(
                2,
                "QUIC handshake done in {:?}, round trip {:?}, remote {}",
                opened.elapsed(),
                conn.rtt(),
                conn.remote_address()
            );
            return Ok(Link::new(Some(conn), Out::Quic(send), Box::new(recv), None));
        }
        let Tier::Stream { name, dial } = &mut self.params.tiers[self.current] else {
            unreachable!("not a UDP tier");
        };
        let stream = dial().await.map_err(|e| format!("{name}: {e}"))?;
        vlog!(2, "{name} stream open after {:?}", opened.elapsed());
        Ok(Link::new(
            None,
            Out::Stream(stream.writer),
            stream.reader,
            stream.guard,
        ))
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
        // The first tier gets little time in the first connection, so that a blocked UDP port is
        // found out quickly. A stream needs a whole `ssh` handshake, which takes seconds on a
        // distant host, so it gets the full time.
        let udp = matches!(self.params.tiers[self.current], Tier::Udp { .. });
        let limit = if udp && !self.linked_once && self.current == 0 && self.params.tiers.len() > 1
        {
            self.params.first_attempt_timeout
        } else {
            self.params.attempt_timeout
        };
        let attempt = timeout(limit, async {
            let link = self.open_link().await?;
            vlog!(2, "sending {}", hello_name(hello));
            link.send(ClientFrame::Hello(hello.clone()));
            let mut link = link;
            let reply = link.reader.next().await;
            Ok::<_, String>((link, reply))
        })
        .await;
        let (link, reply) = match attempt {
            Ok(Ok(opened)) => opened,
            Ok(Err(note)) => {
                vlog!(1, "connecting failed: {note}");
                self.note = Some(sanitize(note.as_bytes(), NOTE_CHARS));
                return Attempt::Retry;
            }
            Err(_) => {
                vlog!(
                    1,
                    "no answer from {} within {limit:?}",
                    self.params.tiers[self.current].name()
                );
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
            // nobody has started the shell yet, so this client does it
            Ok(Some(ServerFrame::Error(ErrorCode::NotStarted)))
                if matches!(hello, Hello::Attach { .. }) && !self.started =>
            {
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
            // a client without history takes over the offsets of the server
            self.output_offset = welcome.output_offset;
            self.input = ReplayBuffer::starting_at(INPUT_BUFFER, welcome.input_offset);
            if welcome.output_offset > 0 {
                // the replay starts in the middle of the stream: cancel a sequence it may cut
                // and reset the colors, so that the first line does not inherit a state
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
            vlog!(
                1,
                "the server forgot {} bytes of output while the client was away",
                welcome.output_offset - output_offset
            );
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
            vlog!(2, "sending {} bytes of input again", chunk.len());
            link.send(ClientFrame::Input(chunk));
        }
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

    /// The frame for typed bytes: compressed when the uplink is what holds the input back (a
    /// paste over a slow link), plain otherwise. Typing is a few bytes and never compressed.
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

    /// Applies typed bytes: input for the shell, or a local escape.
    async fn handle_typed(
        &mut self,
        typed: &[u8],
        link: Option<&mpsc::UnboundedSender<ClientFrame>>,
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
                        let frame = self.input_frame(&bytes);
                        let _ = link.send(frame);
                        // only with a connection: without one the echo cannot come
                        let drawn = self.echo.typed(&bytes, Instant::now());
                        if !drawn.is_empty() {
                            let _ = self.io.output.write_all(&drawn).await;
                            let _ = self.io.output.flush().await;
                        }
                    }
                }
                Action::Detach => {
                    if let Some(link) = link {
                        let _ = link.send(ClientFrame::Detach);
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
        // the channels to the agent live as long as the link
        let mut agent = self
            .params
            .agent
            .clone()
            .map(|socket| Forward::new(socket, link.out.clone()));
        loop {
            // two bytes of reserve: the escape can turn one typed byte into two
            let room = self.input.free().saturating_sub(2).min(self.chunk.len());
            let change_at = change.map(|(_, at)| at);
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
                        // end of input: the shell keeps running, like with `ssh`
                        _ => self.stdin_open = false,
                    }
                }
                frame = link.reader.next() => {
                    let Some(frame) = frame? else {
                        vlog!(1, "the server closed the connection");
                        return Ok(Pumped::Lost);
                    };
                    link.last_heard = Instant::now();
                    probe = None;
                    if std::mem::take(&mut stalled) {
                        self.clear_status().await;
                    }
                    vlog!(3, "<- {}", server_frame_name(&frame));
                    // compressed output is the same as plain output once it is inflated
                    let frame = match frame {
                        ServerFrame::Compressed(packed) => ServerFrame::Output(
                            inflate(&packed)
                                .map_err(|e| format!("the server sent bad compressed output: {e:?}"))?,
                        ),
                        other => other,
                    };
                    match frame {
                        ServerFrame::Output(mut data) => {
                            // Frames that are here already go to the terminal in one write. A
                            // terminal hands out its output in small pieces, and one write (a
                            // trip through the blocking pool) for each would cap the throughput.
                            while data.len() < OUTPUT_BATCH && link.reader.take_ready_output(&mut data) {}
                            // the rest of a sequence that was cancelled on the terminal is dropped
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
                        ServerFrame::AgentOpen(id) => match agent.as_mut() {
                            Some(agent) => agent.open(id).await,
                            // nothing was offered, so a server that opens a channel is wrong
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
                            // The server skipped output that this client was too slow to take.
                            // What was shown ahead of the server goes first, then the notice.
                            vlog!(1, "the server skipped {skipped} bytes of output (slow link)");
                            let taken_back = self.echo.cancel();
                            let _ = self.io.output.write_all(&taken_back).await;
                            self.output_offset += skipped;
                            self.note_gap(skipped).await;
                            link.send(ClientFrame::OutputAck(self.output_offset));
                        }
                        ServerFrame::Exit(status) => {
                            link.send(ClientFrame::Close);
                            return Ok(Pumped::Done(Finished::Exited(exit_code(status))));
                        }
                        ServerFrame::Error(code) => return Err(format!("the server ended the session: {code:?}")),
                        ServerFrame::Welcome(_) => return Err("the server sent a second welcome".to_owned()),
                        // turned into output above
                        ServerFrame::Compressed(_) => unreachable!("inflated above"),
                    }
                }
                Some(()) = self.io.hangups.recv() => {
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
                    // nobody echoed what was shown, as with a password prompt
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
                        vlog!(1, "network change detected: {cause:?}");
                        let strongest = change.map_or(cause, |(old, _)| old.max(cause));
                        change = Some((strongest, tokio::time::Instant::now() + CHANGE_QUIET));
                    }
                    None => self.net = None,
                },
                _ = async { tokio::time::sleep_until(change_at.expect("guarded")).await }, if change_at.is_some() => {
                    let (cause, _) = change.take().expect("guarded");
                    vlog!(1, "the network settled ({cause:?}): moving the socket and probing the server");
                    self.rebind();
                    link.send(ClientFrame::Ping);
                    let wait = if cause == Cause::Woke { PROBE_AFTER_WAKE } else { PROBE_AFTER_CHANGE };
                    probe = Some(tokio::time::Instant::now() + wait);
                }
                _ = async { tokio::time::sleep_until(probe.expect("guarded")).await }, if probe.is_some() => {
                    // the new path did not bring an answer, so the old connection is not worth keeping
                    vlog!(1, "no answer on the new path: giving up the connection");
                    return Ok(Pumped::Lost);
                }
                _ = ping.tick() => {
                    link.send(ClientFrame::Ping);
                    self.unstick_status().await;
                    let silent = link.last_heard.elapsed();
                    let stall_after = link
                        .quic
                        .as_ref()
                        .map_or(STALL_MIN, |conn| (conn.rtt() * 4).clamp(STALL_MIN, STALL_MAX));
                    // QUIC finds a returning network by itself and survives long outages. A
                    // stream has no way to notice that its path is gone, and the `ssh` behind it
                    // would hang, so a long silence ends it and a new one is made.
                    if link.quic.is_none() && silent > self.params.stream_dead_after {
                        vlog!(1, "nothing heard for {silent:?} on a stream: giving it up");
                        return Ok(Pumped::Lost);
                    }
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
