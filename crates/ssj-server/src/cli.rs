//! Command line of `ssj-server`.

use std::ffi::OsString;
use std::ops::RangeInclusive;
use std::path::PathBuf;
use std::time::Duration;

use ssj_net::Congestion;
use ssj_proto::{SessionId, SessionName, is_selector};

use crate::session::SlowClient;

/// Bounds for the output buffer: a size below the lower one cannot hold a screenful of output, and
/// the upper one keeps a runaway setting from using all the memory of a shared host.
pub const MIN_REPLAY_CAPACITY: usize = 64 * 1024;
pub const MAX_REPLAY_CAPACITY: usize = 1 << 30;

/// Lowest port the server may use, so that it never needs privileges.
pub const MIN_PORT: u16 = 1024;

/// What the process is asked to do.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum Mode {
    /// Start a new session.
    #[default]
    Serve,
    /// Ask a running session to accept a new client. The pin comes on stdin.
    Attach(Option<String>),
    /// Print the running sessions.
    List,
    /// Print the version and the protocol.
    Version,
    /// Read a `server.toml` from stdin, print `OK` if it is valid, or the first error.
    CheckConfig,
    /// End a running session.
    /// End these sessions, or the only one when none is named.
    Kill(Vec<String>),
    /// Make the connected client leave; the session keeps running.
    Detach(Vec<String>),
    /// Carry the stream of a client over stdin and stdout, for hosts that allow only the `ssh`
    /// channel.
    Stdio(Option<String>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Args {
    pub mode: Mode,
    /// Name of the session: a new session gets it, and a running one of that name is joined.
    pub name: Option<SessionName>,
    /// Join the only session that has no client, if there is one, instead of starting another.
    pub reuse: bool,
    /// With `reuse` and several sessions without a client, list them and exit with
    /// [`ssj_proto::bootstrap::CHOICE_EXIT_CODE`] instead of starting another.
    pub choose: bool,
    pub port_range: RangeInclusive<u16>,
    /// `None` waits forever.
    pub detached_timeout: Option<Duration>,
    /// Upper limit for a session after a deliberate detach. `None` means no limit.
    pub max_detached: Option<Duration>,
    /// Bytes of output kept for a client that is away, before the oldest are forgotten.
    pub replay_capacity: usize,
    /// Time a connected client may leave a full output buffer alone before it counts as away.
    pub away_after: Duration,
    pub slow_client: SlowClient,
    pub live_backlog: usize,
    /// Compress output when the link is the limit. Off by default.
    pub compress: bool,
    pub congestion: Congestion,
    pub shell: Option<PathBuf>,
    /// Arguments of the shell. The default is `-l`.
    pub shell_args: Vec<OsString>,
    /// Whether the UDP (QUIC) and the TCP transport are offered.
    pub udp: bool,
    pub tcp: bool,
    /// Set by the bootstrap process when it starts the real server.
    pub daemon_child: bool,
    /// The id of the session, chosen by the bootstrap process so that it shows in the arguments of
    /// the daemon (`ps`) and in the log of the client.
    pub session_id: Option<SessionId>,
    /// Which settings the command line gave, since the config file only fills in the others.
    pub explicit: Explicit,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Explicit {
    pub port_range: bool,
    pub detached_timeout: bool,
    pub max_detached: bool,
    pub replay_capacity: bool,
    pub away_after: bool,
    pub slow_client: bool,
    pub live_backlog: bool,
    pub compress: bool,
    pub congestion: bool,
    pub shell: bool,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            mode: Mode::Serve,
            name: None,
            reuse: false,
            choose: false,
            port_range: 60000..=61000,
            detached_timeout: None,
            max_detached: None,
            replay_capacity: 4 << 20,
            away_after: Duration::from_secs(10),
            slow_client: SlowClient::default(),
            live_backlog: DEFAULT_LIVE_BACKLOG,
            compress: false,
            congestion: Congestion::default(),
            shell: None,
            shell_args: Vec::new(),
            udp: true,
            tcp: true,
            daemon_child: false,
            session_id: None,
            explicit: Explicit::default(),
        }
    }
}

impl Args {
    /// Arguments for the child process, so that it behaves like the bootstrap process.
    pub fn to_child_args(&self) -> Vec<OsString> {
        let mut out: Vec<OsString> = vec![
            "--daemon-child".into(),
            "--port-range".into(),
            if self.port_range == (0..=0) {
                "0".to_owned()
            } else {
                format!("{}:{}", self.port_range.start(), self.port_range.end())
            }
            .into(),
            "--detached-timeout".into(),
            self.detached_timeout
                .map_or("none".to_owned(), |t| t.as_secs().to_string())
                .into(),
        ];
        out.extend([
            "--replay-capacity".into(),
            self.replay_capacity.to_string().into(),
        ]);
        out.extend([
            "--away-after".into(),
            self.away_after.as_secs().to_string().into(),
        ]);
        out.extend([
            "--slow-client".into(),
            match self.slow_client {
                SlowClient::Drop => "drop",
                SlowClient::Wait => "wait",
            }
            .into(),
            "--live-backlog".into(),
            self.live_backlog.to_string().into(),
            "--compress".into(),
            if self.compress { "auto" } else { "off" }.into(),
            "--congestion".into(),
            self.congestion.as_str().into(),
        ]);
        out.extend([
            "--max-detached".into(),
            self.max_detached
                .map_or("none".to_owned(), |t| t.as_secs().to_string())
                .into(),
        ]);
        if let Some(shell) = &self.shell {
            out.extend(["--shell".into(), shell.clone().into()]);
        }
        for arg in &self.shell_args {
            out.extend(["--shell-arg".into(), arg.clone()]);
        }
        if let Some(name) = &self.name {
            out.extend(["--name".into(), name.as_str().into()]);
        }
        if let Some(id) = &self.session_id {
            out.extend(["--session-id".into(), id.as_str().into()]);
        }
        if !self.udp {
            out.push("--no-udp".into());
        }
        if !self.tcp {
            out.push("--no-tcp".into());
        }
        out
    }
}

/// `LOW:HIGH`, or `0` for any port the kernel picks.
pub fn parse_port_range(text: &str) -> Result<RangeInclusive<u16>, String> {
    if text == "0" {
        return Ok(0..=0);
    }
    let (low, high) = text.split_once(':').ok_or("expected LOW:HIGH")?;
    let (low, high): (u16, u16) = (
        low.parse().map_err(|_| "LOW is not a port number")?,
        high.parse().map_err(|_| "HIGH is not a port number")?,
    );
    if low < MIN_PORT {
        return Err(format!("ports below {MIN_PORT} need privileges"));
    }
    if low > high {
        return Err("LOW is greater than HIGH".to_owned());
    }
    Ok(low..=high)
}

/// A number of bytes within the allowed bounds.
pub fn parse_replay_capacity(text: &str) -> Result<usize, String> {
    let size: usize = text
        .parse()
        .map_err(|_| "expected a number of bytes".to_owned())?;
    check_replay_capacity(size)
}

pub fn check_replay_capacity(size: usize) -> Result<usize, String> {
    if (MIN_REPLAY_CAPACITY..=MAX_REPLAY_CAPACITY).contains(&size) {
        Ok(size)
    } else {
        Err(format!(
            "must be between {MIN_REPLAY_CAPACITY} and {MAX_REPLAY_CAPACITY} bytes"
        ))
    }
}

/// Default of `live_backlog`: what may wait for a connected client before output is skipped.
pub const DEFAULT_LIVE_BACKLOG: usize = 1 << 20;
const MIN_LIVE_BACKLOG: usize = 16 * 1024;
const MAX_LIVE_BACKLOG: usize = 64 << 20;

pub fn parse_slow_client(text: &str) -> Result<SlowClient, String> {
    match text {
        "drop" => Ok(SlowClient::Drop),
        "wait" => Ok(SlowClient::Wait),
        _ => Err("expected drop or wait".to_owned()),
    }
}

pub fn parse_compress(text: &str) -> Result<bool, String> {
    match text {
        "auto" => Ok(true),
        "off" => Ok(false),
        _ => Err("expected auto or off".to_owned()),
    }
}

pub fn check_live_backlog(size: usize) -> Result<usize, String> {
    if (MIN_LIVE_BACKLOG..=MAX_LIVE_BACKLOG).contains(&size) {
        Ok(size)
    } else {
        Err(format!(
            "must be between {MIN_LIVE_BACKLOG} and {MAX_LIVE_BACKLOG} bytes"
        ))
    }
}

/// Whole seconds, at least one.
pub fn parse_away_after(text: &str) -> Result<Duration, String> {
    match text.parse::<u64>() {
        Ok(secs) if secs > 0 => Ok(Duration::from_secs(secs)),
        _ => Err("expected a number of seconds above 0".to_owned()),
    }
}

pub fn parse_timeout(text: &str) -> Result<Option<Duration>, String> {
    match text {
        "none" | "0" => Ok(None),
        secs => secs
            .parse()
            .map(|s| Some(Duration::from_secs(s)))
            .map_err(|_| "expected seconds or none".to_owned()),
    }
}

pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Args, String> {
    let mut out = Args::default();
    let mut args = args.into_iter().peekable();
    // an optional session id follows `--attach` and `--kill`.
    let session_id = |args: &mut std::iter::Peekable<_>| -> Result<Option<String>, String> {
        let next: Option<&OsString> = args.peek();
        match next.map(|a| a.to_string_lossy().into_owned()) {
            Some(text) if !text.starts_with('-') => {
                if !is_selector(&text) {
                    return Err("not a valid session id or name".to_owned());
                }
                args.next();
                Ok(Some(text))
            }
            _ => Ok(None),
        }
    };
    while let Some(arg) = args.next() {
        let text = arg.to_string_lossy().into_owned();
        match text.as_str() {
            "--attach" => {
                out.mode =
                    Mode::Attach(session_id(&mut args).map_err(|e| format!("--attach: {e}"))?)
            }
            "--kill" => {
                let mut ids = Vec::new();
                while let Some(id) = session_id(&mut args).map_err(|e| format!("--kill: {e}"))? {
                    ids.push(id);
                }
                out.mode = Mode::Kill(ids)
            }
            "--detach" => {
                let mut ids = Vec::new();
                while let Some(id) = session_id(&mut args).map_err(|e| format!("--detach: {e}"))? {
                    ids.push(id);
                }
                out.mode = Mode::Detach(ids)
            }
            "--stdio" => {
                out.mode = Mode::Stdio(session_id(&mut args).map_err(|e| format!("--stdio: {e}"))?)
            }
            "--name" => {
                let value = args.next().ok_or("--name needs a value")?;
                out.name = Some(
                    SessionName::parse(&value.to_string_lossy())
                        .ok_or("--name: 1 to 32 characters of letters, digits, '.', '_', '-'")?,
                );
            }
            "--reuse" => out.reuse = true,
            "--choose" => out.choose = true,
            "--no-udp" => out.udp = false,
            "--no-tcp" => out.tcp = false,
            "--list" => out.mode = Mode::List,
            "--version" => out.mode = Mode::Version,
            "--check-config" => out.mode = Mode::CheckConfig,
            "--port-range" => {
                let value = args.next().ok_or("--port-range needs a value")?;
                out.port_range = parse_port_range(&value.to_string_lossy())
                    .map_err(|e| format!("--port-range: {e}"))?;
                out.explicit.port_range = true;
            }
            "--detached-timeout" => {
                let value = args.next().ok_or("--detached-timeout needs a value")?;
                out.detached_timeout = parse_timeout(&value.to_string_lossy())
                    .map_err(|e| format!("--detached-timeout: {e}"))?;
                out.explicit.detached_timeout = true;
            }
            "--replay-capacity" => {
                let value = args.next().ok_or("--replay-capacity needs a value")?;
                out.replay_capacity = parse_replay_capacity(&value.to_string_lossy())
                    .map_err(|e| format!("--replay-capacity: {e}"))?;
                out.explicit.replay_capacity = true;
            }
            "--away-after" => {
                let value = args.next().ok_or("--away-after needs a value")?;
                out.away_after = parse_away_after(&value.to_string_lossy())
                    .map_err(|e| format!("--away-after: {e}"))?;
                out.explicit.away_after = true;
            }
            "--slow-client" => {
                let value = args.next().ok_or("--slow-client needs a value")?;
                out.slow_client = parse_slow_client(&value.to_string_lossy())
                    .map_err(|e| format!("--slow-client: {e}"))?;
                out.explicit.slow_client = true;
            }
            "--compress" => {
                let value = args.next().ok_or("--compress needs a value")?;
                out.compress = parse_compress(&value.to_string_lossy())
                    .map_err(|e| format!("--compress: {e}"))?;
                out.explicit.compress = true;
            }
            "--congestion" => {
                let value = args.next().ok_or("--congestion needs a value")?;
                out.congestion = Congestion::parse(&value.to_string_lossy())
                    .map_err(|e| format!("--congestion: {e}"))?;
                out.explicit.congestion = true;
            }
            "--live-backlog" => {
                let value = args.next().ok_or("--live-backlog needs a value")?;
                out.live_backlog = value
                    .to_string_lossy()
                    .parse()
                    .map_err(|_| "--live-backlog: expected a number of bytes".to_owned())
                    .and_then(|n| {
                        check_live_backlog(n).map_err(|e| format!("--live-backlog: {e}"))
                    })?;
                out.explicit.live_backlog = true;
            }
            "--max-detached" => {
                let value = args.next().ok_or("--max-detached needs a value")?;
                out.max_detached = parse_timeout(&value.to_string_lossy())
                    .map_err(|e| format!("--max-detached: {e}"))?;
                out.explicit.max_detached = true;
            }
            "--shell" => {
                out.shell = Some(PathBuf::from(args.next().ok_or("--shell needs a value")?));
                out.explicit.shell = true;
            }
            "--shell-arg" => out
                .shell_args
                .push(args.next().ok_or("--shell-arg needs a value")?),
            "--daemon-child" => out.daemon_child = true,
            "--session-id" => {
                let value = args.next().ok_or("--session-id needs a value")?;
                out.session_id = Some(
                    SessionId::parse(&value.to_string_lossy())
                        .ok_or("--session-id: not a session id")?,
                );
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    Ok(out)
}
