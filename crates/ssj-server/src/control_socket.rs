//! The control socket of a running server, and the helpers that talk to it.
//!
//! The socket lives in a private directory and accepts only the user that runs the server. It
//! carries one request line and one reply line per connection.

use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ssj_proto::control::{MAX_LINE, Reply, Request, format_request, parse_reply};

/// Time a peer has to send its request or answer.
pub const IO_TIMEOUT: Duration = Duration::from_secs(5);

/// Directory of the control sockets: `$XDG_RUNTIME_DIR/ssj`, or on systems without it
/// `$XDG_STATE_HOME/ssj/run`. A variable with a relative path is ignored.
pub fn control_dir() -> io::Result<PathBuf> {
    let absolute = |name: &str| {
        std::env::var_os(name)
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
    };
    if let Some(runtime) = absolute("XDG_RUNTIME_DIR") {
        return Ok(runtime.join("ssj"));
    }
    if let Some(state) = absolute("XDG_STATE_HOME") {
        return Ok(state.join("ssj").join("run"));
    }
    let home = absolute("HOME")
        .ok_or_else(|| io::Error::other("neither XDG_RUNTIME_DIR nor HOME is set"))?;
    Ok(home.join(".local/state/ssj/run"))
}

/// Creates the directory with mode 0700 if needed and refuses one that others can enter.
pub fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let mode = fs::metadata(dir)?.permissions().mode();
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
