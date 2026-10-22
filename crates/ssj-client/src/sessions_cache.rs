//! The last known session names of each host (and whether a client is connected), for tab
//! completion. A completion must answer in milliseconds and never prompt, so it reads these files,
//! and a refresh in the background keeps them current.

use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

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

/// Replaces the names of a host, none of them with a client. The file appears whole or not at all.
pub fn store(dir: &Path, origin: &Origin, names: &[String]) -> std::io::Result<()> {
    let entries: Vec<(String, bool)> = names.iter().map(|n| (n.clone(), false)).collect();
    store_states(dir, origin, &entries)
}

/// Replaces the names of a host with the information whether a client is connected to each.
pub fn store_states(
    dir: &Path,
    origin: &Origin,
    entries: &[(String, bool)],
) -> std::io::Result<()> {
    let host = origin.destination.as_str();
    ensure(dir)?;
    let target = dir.join(file_name(host));
    let mut file = tempfile::NamedTempFile::new_in(dir)?;
    writeln!(file, "@{}", origin.destination)?;
    if let Some(port) = origin.port {
        writeln!(file, "@port={port}")?;
    }
    for (name, connected) in entries {
        if *connected {
            writeln!(file, "{name} connected")?;
        } else {
            writeln!(file, "{name}")?;
        }
    }
    file.as_file().sync_all()?;
    file.persist(target).map_err(|e| e.error)?;
    Ok(())
}

/// Names of one host (`[user@]host`), or of all hosts with a list, without repeats.
pub fn names(dir: &Path, host: Option<&str>) -> Vec<String> {
    listed(dir, host, false)
}

/// Like [`names`], and only of the sessions that had a client at the last refresh: the ones that
/// `ssj detach` can act on.
pub fn connected_names(dir: &Path, host: Option<&str>) -> Vec<String> {
    listed(dir, host, true)
}

fn listed(dir: &Path, host: Option<&str>, connected_only: bool) -> Vec<String> {
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
            let (name, state) = line.split_once(' ').unwrap_or((line, ""));
            if connected_only && state != "connected" {
                continue;
            }
            // the file may have been edited, so only a valid selector is passed on.
            if ssj_proto::is_selector(name) && !out.iter().any(|seen| seen == name) {
                out.push(name.to_owned());
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
    // the text goes to `ssh` later, so it must pass the same check as a typed destination.
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

/// Held while one process refreshes the list of a host. Others see it and leave at once. The lock
/// goes with the process, so a refresh that dies leaves nothing behind.
pub struct Refresh {
    _lock: fs::File,
}

impl Refresh {
    pub fn acquire(dir: &Path, host: &str) -> Option<Self> {
        ensure(dir).ok()?;
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(dir.join(format!("{}.lock", file_name(host))))
            .ok()?;
        file.try_lock().ok()?;
        Some(Self { _lock: file })
    }
}
