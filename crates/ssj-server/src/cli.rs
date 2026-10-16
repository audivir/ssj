//! Command line of `ssj-server`.

use std::ffi::OsString;
use std::ops::RangeInclusive;
use std::path::PathBuf;
use std::time::Duration;

/// Lowest port the server may use, so that it never needs privileges.
pub const MIN_PORT: u16 = 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Args {
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
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let text = arg.to_string_lossy().into_owned();
        let mut value = |name: &str| args.next().ok_or(format!("{name} needs a value"));
        match text.as_str() {
            "--port-range" => {
                out.port_range = parse_port_range(&value("--port-range")?.to_string_lossy())
                    .map_err(|e| format!("--port-range: {e}"))?
            }
            "--detached-timeout" => {
                out.detached_timeout =
                    parse_timeout(&value("--detached-timeout")?.to_string_lossy())
                        .map_err(|e| format!("--detached-timeout: {e}"))?
            }
            "--shell" => out.shell = Some(PathBuf::from(value("--shell")?)),
            "--shell-arg" => out.shell_args.push(value("--shell-arg")?),
            "--allow-root" => out.allow_root = true,
            "--daemon-child" => out.daemon_child = true,
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    Ok(out)
}
