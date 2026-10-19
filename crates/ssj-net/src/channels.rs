//! Channels between a local stream and the connection, as used for the forwarded agent.
//!
//! Each channel copies bytes from a local stream (a Unix socket) to the other side and back. The
//! channels are numbered by the side that accepted the local connection, and they live as long as
//! one connection: a new connection starts with none.

use std::collections::HashMap;
use std::future::Future;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Open channels at once. More are refused, so that a program that opens connections in a loop
/// cannot use up the descriptors.
pub const MAX_CHANNELS: usize = 32;
/// Bytes read from the local stream at once.
const CHUNK: usize = 16 * 1024;
/// Chunks for the local stream that may wait. An agent speaks request and reply, so a longer
/// queue means that the program on the other side misbehaves.
const QUEUE: usize = 16;

/// What a channel reports to the connection.
#[derive(Debug, PartialEq, Eq)]
pub enum Piece {
    /// The channel is running. Reported first, once.
    Open,
    /// Bytes that the local stream sent.
    Data(Vec<u8>),
    /// The local stream ended or failed. Reported last, once.
    Closed,
}

struct Channel {
    to_stream: mpsc::Sender<Vec<u8>>,
    task: JoinHandle<()>,
}

/// The channels of one connection. Dropping it closes them all.
#[derive(Default)]
pub struct Channels {
    open: HashMap<u32, Channel>,
}

impl Drop for Channels {
    fn drop(&mut self) {
        for channel in self.open.values() {
            channel.task.abort();
        }
    }
}

impl Channels {
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts channel `id` on `stream`. `report` is called for everything the stream does and
    /// returns false when the connection cannot take more, which ends the channel.
    ///
    /// Returns false, without starting, when [`MAX_CHANNELS`] are open or `id` is in use.
    pub fn open<S, F, Fut>(&mut self, id: u32, stream: S, report: F) -> bool
    where
        S: AsyncRead + AsyncWrite + Send + 'static,
        F: Fn(u32, Piece) -> Fut + Send + 'static,
        Fut: Future<Output = bool> + Send + 'static,
    {
        self.open.retain(|_, channel| !channel.task.is_finished());
        if self.open.len() >= MAX_CHANNELS || self.open.contains_key(&id) {
            return false;
        }
        let (to_stream, from_connection) = mpsc::channel(QUEUE);
        let task = tokio::spawn(copy(id, stream, from_connection, report));
        self.open.insert(id, Channel { to_stream, task });
        true
    }

    /// Passes bytes to the local stream of channel `id`. A channel that is unknown, or that
    /// cannot take them, is closed. Returns whether the bytes were taken.
    pub fn send(&mut self, id: u32, data: Vec<u8>) -> bool {
        let Some(channel) = self.open.get(&id) else {
            return false;
        };
        if channel.to_stream.try_send(data).is_ok() {
            return true;
        }
        self.close(id);
        false
    }

    /// Closes channel `id`, if it is open.
    pub fn close(&mut self, id: u32) {
        if let Some(channel) = self.open.remove(&id) {
            channel.task.abort();
        }
    }

    pub fn is_open(&self, id: u32) -> bool {
        self.open
            .get(&id)
            .is_some_and(|channel| !channel.task.is_finished())
    }
}

async fn copy<S, F, Fut>(
    id: u32,
    stream: S,
    mut from_connection: mpsc::Receiver<Vec<u8>>,
    report: F,
) where
    S: AsyncRead + AsyncWrite,
    F: Fn(u32, Piece) -> Fut,
    Fut: Future<Output = bool>,
{
    if !report(id, Piece::Open).await {
        return;
    }
    let (mut reader, mut writer) = tokio::io::split(stream);
    let mut buf = vec![0u8; CHUNK];
    loop {
        tokio::select! {
            read = reader.read(&mut buf) => match read {
                Ok(n) if n > 0 => {
                    if !report(id, Piece::Data(buf[..n].to_vec())).await {
                        return;
                    }
                }
                _ => {
                    report(id, Piece::Closed).await;
                    return;
                }
            },
            data = from_connection.recv() => match data {
                Some(data) => {
                    if writer.write_all(&data).await.is_err() {
                        report(id, Piece::Closed).await;
                        return;
                    }
                }
                // the other side closed the channel
                None => return,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::duplex;

    use super::*;

    type Seen = mpsc::UnboundedSender<(u32, Piece)>;

    fn reporter() -> (Seen, mpsc::UnboundedReceiver<(u32, Piece)>) {
        mpsc::unbounded_channel()
    }

    fn report(seen: &Seen) -> impl Fn(u32, Piece) -> std::future::Ready<bool> + Send + 'static {
        let seen = seen.clone();
        move |id, piece| std::future::ready(seen.send((id, piece)).is_ok())
    }

    #[tokio::test]
    async fn bytes_go_both_ways_and_the_end_is_reported() {
        let (seen, mut rx) = reporter();
        let (local, mut program) = duplex(1024);
        let mut channels = Channels::new();
        assert!(channels.open(7, local, report(&seen)));
        assert_eq!(rx.recv().await, Some((7, Piece::Open)));

        program.write_all(b"request").await.unwrap();
        assert_eq!(rx.recv().await, Some((7, Piece::Data(b"request".to_vec()))));

        assert!(channels.send(7, b"reply".to_vec()));
        let mut got = [0u8; 5];
        program.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"reply");

        drop(program);
        assert_eq!(rx.recv().await, Some((7, Piece::Closed)));
    }

    #[tokio::test]
    async fn closing_a_channel_ends_the_stream() {
        let (seen, mut rx) = reporter();
        let (local, mut program) = duplex(1024);
        let mut channels = Channels::new();
        assert!(channels.open(1, local, report(&seen)));
        assert_eq!(rx.recv().await, Some((1, Piece::Open)));
        channels.close(1);
        assert_eq!(program.read(&mut [0u8; 8]).await.unwrap(), 0);
        assert!(!channels.send(1, b"late".to_vec()));
    }

    #[tokio::test]
    async fn the_number_of_channels_is_limited() {
        let (seen, _rx) = reporter();
        let mut channels = Channels::new();
        let mut keep = Vec::new();
        for id in 0..MAX_CHANNELS as u32 {
            let (local, program) = duplex(64);
            keep.push(program);
            assert!(channels.open(id, local, report(&seen)));
        }
        let (local, _program) = duplex(64);
        assert!(!channels.open(999, local, report(&seen)));
        // a channel that ended makes room
        drop(keep.pop());
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let (local, _program) = duplex(64);
        assert!(channels.open(1000, local, report(&seen)));
    }

    #[tokio::test]
    async fn an_id_in_use_is_refused() {
        let (seen, _rx) = reporter();
        let mut channels = Channels::new();
        let (a, _pa) = duplex(64);
        let (b, _pb) = duplex(64);
        assert!(channels.open(3, a, report(&seen)));
        assert!(!channels.open(3, b, report(&seen)));
    }
}
