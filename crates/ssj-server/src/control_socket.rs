//! The control socket of a running server, and the helpers that talk to it.
//!
//! The socket lives in a private directory and accepts only the user that runs the server. It
//! carries one request line and one reply line per connection.

use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ssj_proto::control::{MAX_LINE, Reply, Request, format_request, parse_reply};

/// Time a peer has to send its request or answer.
pub const IO_TIMEOUT: Duration = Duration::from_secs(5);

/// Longest path of a socket that is used. The kernel takes 104 bytes on macOS and 108 on Linux,
/// both with the NUL byte.
const MAX_SOCKET_PATH: usize = 100;
/// `/`, a session id of 16 characters, and `.agent` (the longer of the two endings).
const SOCKET_NAME: usize = 1 + 16 + 6;

/// Directory of the control sockets for this user on this host.
pub fn control_dir() -> io::Result<PathBuf> {
    let absolute = |name: &str| {
        std::env::var_os(name)
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
    };
    dir_for(
        absolute("XDG_RUNTIME_DIR"),
        absolute("XDG_STATE_HOME"),
        absolute("HOME"),
        &host_dir_name(),
        rustix::process::getuid().as_raw(),
    )
}

/// The directory for the given environment: `$XDG_RUNTIME_DIR/ssj`, or on systems without it
/// `$XDG_STATE_HOME/ssj/run/<host>` (default `~/.local/state`).
///
/// The runtime directory belongs to one host. The state directory may be shared by several (a
/// home directory on a network file system), and a socket of another host would look like a dead
/// one here, so each host gets its own directory. A path that would be too long for a socket
/// moves to `/tmp/ssj-<uid>`.
pub fn dir_for(
    runtime: Option<PathBuf>,
    state: Option<PathBuf>,
    home: Option<PathBuf>,
    host: &str,
    uid: u32,
) -> io::Result<PathBuf> {
    let preferred = if let Some(runtime) = runtime {
        runtime.join("ssj")
    } else if let Some(state) = state {
        state.join("ssj").join("run").join(host)
    } else {
        let home =
            home.ok_or_else(|| io::Error::other("neither XDG_RUNTIME_DIR nor HOME is set"))?;
        home.join(".local/state/ssj/run").join(host)
    };
    if preferred.as_os_str().len() + SOCKET_NAME <= MAX_SOCKET_PATH {
        Ok(preferred)
    } else {
        Ok(PathBuf::from(format!("/tmp/ssj-{uid}")))
    }
}

/// The name of this host as a single safe path component.
pub fn host_dir_name() -> String {
    let name = rustix::system::uname()
        .nodename()
        .to_string_lossy()
        .into_owned();
    let name: String = name
        .chars()
        .take(64)
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    // no `.` or `..`, and nothing hidden
    match name.trim_start_matches('.') {
        "" => "host".to_owned(),
        trimmed => trimmed.to_owned(),
    }
}

/// Creates the directory with mode 0700 if needed. It refuses a symbolic link, a directory of
/// another user (somebody could have made it first in a shared place such as `/tmp`), and one that
/// others can enter.
pub fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let meta = fs::symlink_metadata(dir)?;
    if !meta.is_dir() {
        return Err(io::Error::other(format!(
            "{} is not a directory",
            dir.display()
        )));
    }
    if meta.uid() != rustix::process::getuid().as_raw() {
        return Err(io::Error::other(format!(
            "{} belongs to another user",
            dir.display()
        )));
    }
    let mode = meta.permissions().mode();
    if mode & 0o077 != 0 {
        return Err(io::Error::other(format!(
            "{} is accessible to others (mode {:o})",
            dir.display(),
            mode & 0o777
        )));
    }
    Ok(())
}

pub fn socket_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.sock"))
}

/// The socket that carries the agent of the client to the programs of the session.
pub fn agent_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.agent"))
}

/// Removes the socket file when dropped.
pub struct SocketFile(pub PathBuf);

impl Drop for SocketFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Sends `request` to the socket at `path` and reads the single reply line.
pub fn ask(path: &Path, request: &Request) -> io::Result<Reply> {
    let mut stream = StdUnixStream::connect(path)?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    stream.write_all(format_request(request).as_bytes())?;
    let mut line = Vec::new();
    BufReader::new(stream.take(MAX_LINE as u64)).read_until(b'\n', &mut line)?;
    parse_reply(&line).map_err(|_| io::Error::other("the server sent an invalid reply"))
}

/// A session found in the control directory.
pub struct Found {
    pub path: PathBuf,
    pub info: ssj_proto::control::SessionInfo,
}

/// Asks every socket in `dir` for its status. Sockets nobody listens on are removed.
pub fn find_sessions(dir: &Path) -> Vec<Found> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "sock") {
            continue;
        }
        match ask(&path, &Request::Status) {
            Ok(Reply::Info(info)) => found.push(Found { path, info }),
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                ) =>
            {
                // a leftover of a server that did not end cleanly
                let _ = fs::remove_file(&path);
            }
            _ => {}
        }
    }
    found.sort_by(|a, b| a.info.id.as_str().cmp(b.info.id.as_str()));
    found
}
