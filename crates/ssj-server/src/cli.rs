//! Command line of `ssj-server`.

use std::ffi::OsString;
use std::ops::RangeInclusive;
use std::path::PathBuf;
use std::time::Duration;

use ssj_proto::SessionId;

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
    /// End a running session.
    Kill(Option<SessionId>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Args {
    pub mode: Mode,
    pub port_range: RangeInclusive<u16>,
    /// `None` waits forever.
    pub detached_timeout: Option<Duration>,
    pub shell: Option<PathBuf>,
    /// Arguments of the shell. The default is `-l`.
    pub shell_args: Vec<OsString>,
    pub allow_root: bool,
    /// Set by the bootstrap process when it starts the real server.
    pub daemon_child: bool,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            mode: Mode::Serve,
            port_range: 60000..=61000,
            detached_timeout: Some(Duration::from_secs(7 * 24 * 3600)),
            shell: None,
            shell_args: Vec::new(),
            allow_root: false,
            daemon_child: false,
        }
    }
}

impl Args {
    /// Arguments for the child process, so that it behaves like the bootstrap process.
    pub fn to_child_args(&self) -> Vec<OsString> {
        let mut out: Vec<OsString> = vec![
            "--daemon-child".into(),
            "--port-range".into(),
            format!("{}:{}", self.port_range.start(), self.port_range.end()).into(),
            "--detached-timeout".into(),
            self.detached_timeout
                .map_or("none".to_owned(), |t| t.as_secs().to_string())
                .into(),
        ];
        if let Some(shell) = &self.shell {
            out.extend(["--shell".into(), shell.clone().into()]);
        }
        for arg in &self.shell_args {
            out.extend(["--shell-arg".into(), arg.clone()]);
        }
        if self.allow_root {
            out.push("--allow-root".into());
        }
        out
    }
}

pub fn parse_port_range(text: &str) -> Result<RangeInclusive<u16>, String> {
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
            "--port-range" => {
                let value = args.next().ok_or("--port-range needs a value")?;
                out.port_range = parse_port_range(&value.to_string_lossy())
                    .map_err(|e| format!("--port-range: {e}"))?;
            }
            "--detached-timeout" => {
                let value = args.next().ok_or("--detached-timeout needs a value")?;
                out.detached_timeout = parse_timeout(&value.to_string_lossy())
                    .map_err(|e| format!("--detached-timeout: {e}"))?;
            }
            "--shell" => {
                out.shell = Some(PathBuf::from(args.next().ok_or("--shell needs a value")?))
            }
            "--shell-arg" => out
                .shell_args
                .push(args.next().ok_or("--shell-arg needs a value")?),
            "--allow-root" => out.allow_root = true,
            "--daemon-child" => out.daemon_child = true,
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    Ok(out)
}
