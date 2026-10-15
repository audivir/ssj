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
pub use ssj_proto::{SessionId, WindowSize};

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
