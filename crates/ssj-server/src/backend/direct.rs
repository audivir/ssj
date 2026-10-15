use std::io;
use std::pin::Pin;
use std::process::ExitStatus;
use std::task::{Context, Poll};

use pty_process::Size;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::process::Child;

use super::{Backend, SessionId, Shell, SpawnSpec, WindowSize};

/// Backend that runs the shell as a child of the server, which ends with the server.
#[derive(Clone, Copy, Debug, Default)]
pub struct Direct;

/// Shell on a PTY owned by the server process.
pub struct DirectShell {
    pty: pty_process::Pty,
    child: Child,
}

fn to_size(size: WindowSize) -> Size {
    Size::new(size.rows, size.cols)
}

fn to_io(err: pty_process::Error) -> io::Error {
    io::Error::other(err)
}

impl Backend for Direct {
    type Shell = DirectShell;

    const SURVIVES_SERVER_RESTART: bool = false;

    async fn spawn(&self, _id: &SessionId, spec: SpawnSpec) -> io::Result<DirectShell> {
        let (pty, pts) = pty_process::open().map_err(to_io)?;
        pty.resize(to_size(spec.size)).map_err(to_io)?;
        let mut command = pty_process::Command::new(&spec.program)
            .args(&spec.args)
            .env_clear()
            .envs(spec.env.iter().map(|(k, v)| (k, v)))
            .kill_on_drop(true);
        if let Some(cwd) = &spec.cwd {
            command = command.current_dir(cwd);
        }
        let child = command.spawn(pts).map_err(to_io)?;
        Ok(DirectShell { pty, child })
    }

    async fn attach(&self, _id: &SessionId) -> io::Result<Option<DirectShell>> {
        Ok(None)
    }
}

impl Shell for DirectShell {
    fn resize(&self, size: WindowSize) -> io::Result<()> {
        self.pty.resize(to_size(size)).map_err(to_io)
    }

    async fn wait(&mut self) -> io::Result<ExitStatus> {
        self.child.wait().await
    }
}

impl AsyncRead for DirectShell {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.pty).poll_read(cx, buf)
    }
}

impl AsyncWrite for DirectShell {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.pty).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.pty).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.pty).poll_shutdown(cx)
    }
}
