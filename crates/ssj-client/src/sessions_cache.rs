//! The last known session names of each host, for tab completion. A completion must answer in
//! milliseconds and never prompt, so it reads these files, and a refresh in the background keeps
//! them current.

use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

/// A lock older than this is left over from a refresh that died.
const LOCK_STALE: Duration = Duration::from_secs(30);

/// The list of a host is refreshed when it is older than this.
pub const REFRESH_AFTER: Duration = Duration::from_secs(10);

/// `$XDG_CACHE_HOME/ssj/sessions`, or `~/.cache/ssj/sessions`.
pub fn dir() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_CACHE_HOME").filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(std::env::var_os("HOME").filter(|v| !v.is_empty())?).join(".cache"),
    };
    base.is_absolute()
        .then(|| base.join("ssj").join("sessions"))
}

/// File name for a host: nothing that could leave the directory.
fn file_name(host: &str) -> String {
    let name: String = host
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':') {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("{}.list", name.trim_start_matches('.'))
}

fn ensure(dir: &Path) -> std::io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

/// Where a list came from, so that a refresh can ask the same host again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Origin {
    /// `[user@]host` as given to `ssh`.
    pub destination: String,
    pub port: Option<u16>,
}

/// Replaces the names of a host. The file appears whole or not at all.
pub fn store(dir: &Path, origin: &Origin, names: &[String]) -> std::io::Result<()> {
    let host = origin.destination.as_str();
    ensure(dir)?;
    let target = dir.join(file_name(host));
    let temp = dir.join(format!(".{}.{}", file_name(host), std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temp)?;
    writeln!(file, "@{}", origin.destination)?;
    if let Some(port) = origin.port {
        writeln!(file, "@port={port}")?;
    }
    for name in names {
        writeln!(file, "{name}")?;
    }
    file.sync_all()?;
    fs::rename(temp, target)
}

/// Names of one host (`[user@]host`), or of all hosts with a list, without repeats.
pub fn names(dir: &Path, host: Option<&str>) -> Vec<String> {
    let files: Vec<PathBuf> = match host {
        Some(host) => vec![dir.join(file_name(host))],
        None => fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| is_list(path))
            .collect(),
    };
    let mut out: Vec<String> = Vec::new();
    for file in files {
        for line in fs::read_to_string(file).unwrap_or_default().lines() {
            // the file may have been edited, so only a valid selector is passed on
            if ssj_proto::is_selector(line) && !out.iter().any(|seen| seen == line) {
                out.push(line.to_owned());
            }
        }
    }
    out
}

fn is_list(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "list")
        && path
            .file_name()
            .is_some_and(|n| !n.to_string_lossy().starts_with('.'))
}

/// Origins whose list is older than the given age.
pub fn stale_origins(dir: &Path, older_than: Duration) -> Vec<Origin> {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| is_list(&entry.path()))
        .filter(|entry| {
            entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| SystemTime::now().duration_since(t).ok())
                .is_none_or(|age| age > older_than)
        })
        .filter_map(|entry| origin_of(&fs::read_to_string(entry.path()).ok()?))
        .collect()
}

fn origin_of(text: &str) -> Option<Origin> {
    let mut lines = text.lines();
    let destination = lines.next()?.strip_prefix('@')?.to_owned();
    // the text goes to `ssh` later, so it must pass the same check as a typed destination
    let parsed = ssj_proto::destination::parse(&destination).ok()?;
    if parsed.port.is_some() || parsed.target() != destination {
        return None;
    }
    let port = lines
        .next()
        .and_then(|line| line.strip_prefix("@port="))
        .and_then(|port| port.parse().ok());
    Some(Origin { destination, port })
}

/// Held while one process refreshes the list of a host. Others see it and leave at once.
pub struct Refresh {
    path: PathBuf,
}

impl Refresh {
    pub fn acquire(dir: &Path, host: &str) -> Option<Self> {
        ensure(dir).ok()?;
        let path = dir.join(format!("{}.lock", file_name(host)));
        for _ in 0..2 {
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
            {
                Ok(_) => return Some(Self { path }),
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                    let old = fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| SystemTime::now().duration_since(t).ok())
                        .is_some_and(|age| age > LOCK_STALE);
                    if !old {
                        return None;
                    }
                    let _ = fs::remove_file(&path);
                }
                Err(_) => return None,
            }
        }
        None
    }
}

impl Drop for Refresh {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}
