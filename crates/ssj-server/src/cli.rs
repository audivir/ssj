//! Command line of `ssj-server`.

use std::ffi::OsString;
use std::ops::RangeInclusive;
use std::path::PathBuf;
use std::time::Duration;

use ssj_proto::SessionId;

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
    Attach(Option<SessionId>),
    /// Print the running sessions.
    List,
    /// Print the version and the protocol.
    Version,
    /// End a running session.
    Kill(Option<SessionId>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Args {
    pub mode: Mode,
    pub port_range: RangeInclusive<u16>,
    /// `None` waits forever.
    pub detached_timeout: Option<Duration>,
    /// Upper limit for a session after a deliberate detach. `None` means no limit.
    pub max_detached: Option<Duration>,
    /// Bytes of output kept for a client that is away, before the oldest are forgotten.
    pub replay_capacity: usize,
    pub shell: Option<PathBuf>,
    /// Arguments of the shell. The default is `-l`.
    pub shell_args: Vec<OsString>,
    /// Set by the bootstrap process when it starts the real server.
    pub daemon_child: bool,
    /// Which settings the command line gave, since the config file only fills in the others.
    pub explicit: Explicit,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Explicit {
    pub port_range: bool,
    pub detached_timeout: bool,
    pub max_detached: bool,
    pub replay_capacity: bool,
    pub shell: bool,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            mode: Mode::Serve,
            port_range: 60000..=61000,
            detached_timeout: None,
            max_detached: None,
            replay_capacity: 4 << 20,
            shell: None,
            shell_args: Vec::new(),
            daemon_child: false,
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
    // an optional session id follows `--attach` and `--kill`
    let session_id = |args: &mut std::iter::Peekable<_>| -> Result<Option<SessionId>, String> {
        let next: Option<&OsString> = args.peek();
        match next.map(|a| a.to_string_lossy().into_owned()) {
            Some(text) if !text.starts_with('-') => {
                let id = SessionId::parse(&text).ok_or("not a valid session id")?;
                args.next();
                Ok(Some(id))
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
                out.mode = Mode::Kill(session_id(&mut args).map_err(|e| format!("--kill: {e}"))?)
            }
            "--list" => out.mode = Mode::List,
            "--version" => out.mode = Mode::Version,
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
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    Ok(out)
}
