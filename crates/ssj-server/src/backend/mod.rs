//! Places where the shell of a session can live.
//!
//! The session logic only talks to [`Backend`] and [`Shell`]. A backend that keeps the shell alive
//! when the server process ends (such as a holder process or a multiplexer) can be added without
//! touching the session logic.

mod direct;

use std::ffi::OsString;
use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::process::ExitStatus;

use tokio::io::{AsyncRead, AsyncWrite};

pub use direct::Direct;

/// Terminal dimensions in character cells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowSize {
    pub rows: u16,
    pub cols: u16,
}

/// Command, environment, and terminal size of a shell to start.
///
/// The environment is complete: backends must not add variables of the server process.
#[derive(Clone, Debug)]
pub struct SpawnSpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub env: Vec<(OsString, OsString)>,
    pub cwd: Option<PathBuf>,
    pub size: WindowSize,
}

/// Identifier of a session, restricted to characters that are safe in file names and commands.
///
/// It names control sockets and multiplexer sessions, so it must never contain path separators,
/// dots, whitespace, or option prefixes.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SessionId(String);

impl SessionId {
    const MIN_LEN: usize = 8;
    const MAX_LEN: usize = 64;

    /// Accepts lowercase ASCII letters and digits, between 8 and 64 characters.
    pub fn parse(id: &str) -> Option<Self> {
        let valid = (Self::MIN_LEN..=Self::MAX_LEN).contains(&id.len())
            && id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
        valid.then(|| Self(id.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Byte stream to a running shell, with its terminal controls.
///
/// Reading yields the output of the shell and ends when the shell has exited. Writing sends input.
pub trait Shell: AsyncRead + AsyncWrite + Send + Unpin {
    /// Changes the terminal size seen by the shell.
    fn resize(&self, size: WindowSize) -> io::Result<()>;

    /// Waits for the shell to exit.
    fn wait(&mut self) -> impl Future<Output = io::Result<ExitStatus>> + Send;
}

/// Interface for creating shells and finding ones that outlived the server process.
pub trait Backend: Send + Sync + 'static {
    type Shell: Shell;

    /// Whether shells of this backend keep running when the server process ends.
    const SURVIVES_SERVER_RESTART: bool;

    /// Starts a shell for session `id`.
    fn spawn(
        &self,
        id: &SessionId,
        spec: SpawnSpec,
    ) -> impl Future<Output = io::Result<Self::Shell>> + Send;

    /// Reconnects to the shell of session `id` that outlived a previous server process.
    ///
    /// Returns `None` if there is no such shell, which is always the case for backends with
    /// `SURVIVES_SERVER_RESTART` set to `false`.
    fn attach(
        &self,
        id: &SessionId,
    ) -> impl Future<Output = io::Result<Option<Self::Shell>>> + Send;
}
