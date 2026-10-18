//! Process setup of the server: detaching from the `ssh` session and removing leaks.

use std::fs::File;
use std::io::{self, Read};
use std::os::fd::AsFd;

use rustix::process::{Resource, Rlimit, setrlimit, setsid};

/// Starts a new session, so that the end of the `ssh` channel does not touch the server.
pub fn detach_from_session() {
    // fails only if the process leads a process group already, which is harmless here
    let _ = setsid();
}

/// Prevents core dumps, which could contain keys and terminal contents.
pub fn disable_core_dumps() -> io::Result<()> {
    setrlimit(
        Resource::Core,
        Rlimit {
            current: Some(0),
            maximum: Some(0),
        },
    )?;
    Ok(())
}

/// Points stdin, stdout, and stderr at `/dev/null`, which releases the pipes of the `ssh` channel.
pub fn silence_stdio() -> io::Result<()> {
    let null = File::options().read(true).write(true).open("/dev/null")?;
    rustix::stdio::dup2_stdin(null.as_fd())?;
    rustix::stdio::dup2_stdout(null.as_fd())?;
    rustix::stdio::dup2_stderr(null.as_fd())?;
    Ok(())
}

/// The descriptors above 2 that this process has open, from the list the system keeps.
fn listed_descriptors() -> Option<Vec<i32>> {
    ["/proc/self/fd", "/dev/fd"].iter().find_map(|dir| {
        let entries = std::fs::read_dir(dir).ok()?;
        Some(
            entries
                .flatten()
                .filter_map(|entry| entry.file_name().to_str()?.parse::<i32>().ok())
                .filter(|fd| *fd > 2)
                .collect(),
        )
    })
}

/// Closes every descriptor above 2 that the parent process left open, so that neither the
/// server nor the shell it starts holds a socket or a file of someone else. `sshd` hands over only
/// 0 to 2, but a wrapper script or another `ssh` may not. Call it before anything is opened.
#[allow(unsafe_code)]
pub fn close_inherited_descriptors() {
    let candidates = listed_descriptors().unwrap_or_else(|| {
        // no list on this system: try every number up to the limit, with a ceiling
        let limit = rustix::process::getrlimit(Resource::Nofile)
            .current
            .unwrap_or(1024)
            .min(65_536);
        (3..i32::try_from(limit).unwrap_or(i32::MAX)).collect()
    });
    for fd in candidates {
        // SAFETY: nothing else in this process owns a descriptor at this point (the function runs
        // first in `main`, and the handle of the directory listing is gone), so closing by number
        // cannot pull a descriptor from under a live `File` or socket. A number that is not open
        // (the listing's own handle) fails with EBADF, which `close` ignores.
        unsafe { rustix::io::close(fd) };
    }
}

/// Fills `buf` from the random source of the operating system.
pub fn random_bytes(buf: &mut [u8]) -> io::Result<()> {
    File::open("/dev/urandom")?.read_exact(buf)
}

/// Random identifier of 16 characters from `a-z` and `2-7`, which is valid for `SessionId`.
pub fn random_session_id() -> io::Result<String> {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut bytes = [0u8; 16];
    random_bytes(&mut bytes)?;
    Ok(bytes
        .iter()
        .map(|b| ALPHABET[usize::from(b & 31)] as char)
        .collect())
}

pub fn is_root() -> bool {
    rustix::process::getuid().is_root()
}
