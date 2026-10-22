//! Frames on a connection: reading them from a stream, writing them, and naming them for `-v`.

use ssj_proto::{ClientFrame, Hello, Peek, ServerFrame, decode_server, peek_server};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::time::timeout;

use super::{FLUSH_TIMEOUT, Reader, Writer};
use crate::vlog;

/// Bytes asked from the stream at a time.
const READ_SIZE: usize = 64 * 1024;

/// Reads server frames from a stream, keeping bytes of incomplete frames between calls.
pub(super) struct FrameReader {
    recv: Reader,
    /// Received bytes. The ones before `start` are used already; they are dropped when more is
    /// read, once per read and not once per frame.
    buf: Vec<u8>,
    start: usize,
}

impl FrameReader {
    pub(super) fn new(recv: Reader) -> Self {
        Self {
            recv,
            buf: Vec::with_capacity(2 * READ_SIZE),
            start: 0,
        }
    }

    fn pending(&self) -> &[u8] {
        &self.buf[self.start..]
    }

    fn consume(&mut self, used: usize) {
        self.start += used;
        if self.start == self.buf.len() {
            self.buf.clear();
            self.start = 0;
        }
    }

    /// Next frame, or `None` when the stream ended.
    pub(super) async fn next(&mut self) -> Result<Option<ServerFrame>, String> {
        loop {
            match decode_server(self.pending()) {
                Ok(Some((frame, used))) => {
                    self.consume(used);
                    return Ok(Some(frame));
                }
                Ok(None) => {}
                Err(err) => return Err(format!("the server sent an invalid frame: {err:?}")),
            }
            if self.start > 0 {
                self.buf.drain(..self.start);
                self.start = 0;
            }
            // read straight into the spare room of the buffer. This future is dropped when
            // another branch of the `select!` wins, so the length of the buffer must only grow
            // when a read has completed: `read_buf` does that, and padding the buffer with zeros
            // before the read would leave the zeros in it.
            self.buf.reserve(READ_SIZE);
            match self.recv.read_buf(&mut self.buf).await {
                Ok(0) | Err(_) => return Ok(None),
                Ok(_) => {}
            }
        }
    }

    /// Appends the data of the next frame to `out` if it is a complete output frame that is
    /// waiting already. Any other frame stays where it is, for [`Self::next`].
    pub(super) fn take_ready_output(&mut self, out: &mut Vec<u8>) -> bool {
        match peek_server(self.pending()) {
            Ok(Peek::Output { data, used }) => {
                let start = self.start;
                out.extend_from_slice(&self.buf[start + data.start..start + data.end]);
                self.consume(used);
                true
            }
            _ => false,
        }
    }

    /// Whether a complete frame is waiting, so that acknowledgements can be sent per batch.
    pub(super) fn frame_ready(&self) -> bool {
        !matches!(peek_server(self.pending()), Ok(Peek::Incomplete))
    }
}

/// The sending side of a connection.
pub(super) enum Out {
    Quic(quinn::SendStream),
    Stream(Writer),
}

impl Out {
    async fn write_all(&mut self, bytes: &[u8]) -> bool {
        match self {
            Self::Quic(send) => send.write_all(bytes).await.is_ok(),
            Self::Stream(writer) => writer.write_all(bytes).await.is_ok(),
        }
    }

    /// Ends the stream after the data, and waits until the peer has it.
    async fn finish(&mut self) {
        match self {
            Self::Quic(send) => {
                let _ = send.finish();
                // the peer must have the last frame before the process ends.
                let _ = timeout(FLUSH_TIMEOUT, send.stopped()).await;
            }
            Self::Stream(writer) => {
                let _ = timeout(FLUSH_TIMEOUT, async {
                    let _ = writer.flush().await;
                    let _ = writer.shutdown().await;
                })
                .await;
            }
        }
    }
}

/// Sends frames from a channel, so that a stalled stream never blocks reading.
pub(super) async fn write_frames(mut out: Out, mut frames: mpsc::UnboundedReceiver<ClientFrame>) {
    let mut bytes = Vec::new();
    while let Some(frame) = frames.recv().await {
        vlog!(3, "-> {}", client_frame_name(&frame));
        bytes.clear();
        if frame.encode(&mut bytes).is_err() {
            return;
        }
        // frames that are queued at the same moment go out in one write, so that a hop with
        // Nagle's algorithm does not hold the second of two small packets back for the delayed
        // ACK of the first.
        tokio::task::yield_now().await;
        while bytes.len() < 64 * 1024 {
            match frames.try_recv() {
                Ok(next) => {
                    vlog!(3, "-> {}", client_frame_name(&next));
                    if next.encode(&mut bytes).is_err() {
                        return;
                    }
                }
                Err(_) => break,
            }
        }
        if !out.write_all(&bytes).await {
            return;
        }
        // a stream with a buffer must not hold a keystroke back.
        if let Out::Stream(writer) = &mut out
            && writer.flush().await.is_err()
        {
            return;
        }
    }
    out.finish().await;
}

/// The kind of a hello, for the debug output.
pub(super) fn hello_name(hello: &Hello) -> &'static str {
    match hello {
        Hello::Start { .. } => "start",
        Hello::Resume { .. } => "resume",
        Hello::Attach { .. } => "attach",
    }
}

/// A client frame by kind and size, for the debug output. Never the content.
pub(super) fn client_frame_name(frame: &ClientFrame) -> String {
    match frame {
        ClientFrame::Hello(hello) => hello_name(hello).to_owned(),
        ClientFrame::Input(data) => format!("input {} bytes", data.len()),
        ClientFrame::Compressed(data) => format!("compressed input {} bytes", data.len()),
        ClientFrame::Resize(size) => format!("resize {}x{}", size.cols(), size.rows()),
        ClientFrame::OutputAck(offset) => format!("output ack {offset}"),
        ClientFrame::Close => "close".to_owned(),
        ClientFrame::Detach => "detach".to_owned(),
        ClientFrame::Hangup => "hangup".to_owned(),
        ClientFrame::Ping => "ping".to_owned(),
        ClientFrame::AgentOffer => "agent offer".to_owned(),
        ClientFrame::Agent(id, data) => format!("agent {id} {} bytes", data.len()),
        ClientFrame::AgentClose(id) => format!("agent close {id}"),
    }
}

/// A server frame by kind and size, for the debug output.
pub(super) fn server_frame_name(frame: &ServerFrame) -> String {
    match frame {
        ServerFrame::Welcome(w) => format!(
            "welcome, input offset {}, output offset {}",
            w.input_offset, w.output_offset
        ),
        ServerFrame::Output(data) => format!("output {} bytes", data.len()),
        ServerFrame::Compressed(data) => format!("compressed output {} bytes", data.len()),
        ServerFrame::InputAck(offset) => format!("input ack {offset}"),
        ServerFrame::Exit(status) => format!("exit {status:?}"),
        ServerFrame::Error(code) => format!("error {code:?}"),
        ServerFrame::Pong => "pong".to_owned(),
        ServerFrame::Gap(skipped) => format!("gap {skipped} bytes"),
        ServerFrame::Detached => "detached by `ssj detach`".to_owned(),
        ServerFrame::AgentOpen(id) => format!("agent open {id}"),
        ServerFrame::Agent(id, data) => format!("agent {id} {} bytes", data.len()),
        ServerFrame::AgentClose(id) => format!("agent close {id}"),
    }
}
