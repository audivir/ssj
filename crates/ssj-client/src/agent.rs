//! The ssh agent of this machine, offered to the programs of the session.
//!
//! The server makes a socket for them, and every connection to it becomes a channel to a new
//! connection to the agent here. Only the requests that list the keys and sign are passed on, so
//! the host cannot add, remove, or lock keys, and the host never sees a private key either way.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use ssj_net::channels::{Channels, Piece};

use crate::vlog;
use ssj_proto::ClientFrame;
use ssj_proto::frame::MAX_DATA_LEN;
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio::time::timeout;

/// Message types of the agent protocol that are passed on.
const REQUEST_IDENTITIES: u8 = 11;
const SIGN_REQUEST: u8 = 13;
/// The answer to a request that is not passed on.
const FAILURE: [u8; 5] = [0, 0, 0, 1, 5];
/// Largest message, the limit of the agent protocol.
const MAX_MESSAGE: usize = 256 * 1024;
/// Time the agent has to accept a connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// What [`Filter::feed`] made of some bytes.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Filtered {
    /// Complete messages for the agent.
    pub forward: Vec<u8>,
    /// Answers to messages that were refused.
    pub reply: Vec<u8>,
    /// The bytes are not a valid message stream, and the channel must end.
    pub broken: bool,
}

/// Splits what a program sends into messages and keeps those that are allowed.
#[derive(Default)]
pub struct Filter {
    pending: Vec<u8>,
}

impl Filter {
    pub fn feed(&mut self, data: &[u8]) -> Filtered {
        self.pending.extend_from_slice(data);
        let mut out = Filtered::default();
        while let Some(header) = self.pending.get(..4) {
            let len = u32::from_be_bytes(header.try_into().unwrap()) as usize;
            if len == 0 || len > MAX_MESSAGE {
                out.broken = true;
                break;
            }
            if self.pending.len() < 4 + len {
                break;
            }
            let message: Vec<u8> = self.pending.drain(..4 + len).collect();
            if matches!(message[4], REQUEST_IDENTITIES | SIGN_REQUEST) {
                out.forward.extend_from_slice(&message);
            } else {
                out.reply.extend_from_slice(&FAILURE);
            }
        }
        out
    }
}

/// The channels to the agent of one connection.
pub struct Forward {
    socket: PathBuf,
    channels: Channels,
    filters: HashMap<u32, Filter>,
    out: mpsc::UnboundedSender<ClientFrame>,
}

impl Forward {
    pub fn new(socket: PathBuf, out: mpsc::UnboundedSender<ClientFrame>) -> Self {
        Self {
            socket,
            channels: Channels::new(),
            filters: HashMap::new(),
            out,
        }
    }

    /// The server opened channel `id`: connects to the agent. If that fails, the server is told
    /// that the channel is closed.
    pub async fn open(&mut self, id: u32) {
        self.filters
            .retain(|known, _| self.channels.is_open(*known));
        let stream = timeout(CONNECT_TIMEOUT, UnixStream::connect(&self.socket)).await;
        let out = self.out.clone();
        let opened = match stream {
            Ok(Ok(stream)) => self.channels.open(id, stream, move |id, piece| {
                let frame = match piece {
                    Piece::Open => None,
                    Piece::Data(data) => Some(ClientFrame::Agent(id, data)),
                    Piece::Closed => Some(ClientFrame::AgentClose(id)),
                };
                let sent = frame.is_none_or(|frame| out.send(frame).is_ok());
                std::future::ready(sent)
            }),
            _ => false,
        };
        if opened {
            vlog!(2, "agent channel {id} opened");
            self.filters.insert(id, Filter::default());
        } else {
            vlog!(
                1,
                "cannot open agent channel {id} to {}",
                self.socket.display()
            );
            let _ = self.out.send(ClientFrame::AgentClose(id));
        }
    }

    /// Bytes of a program on channel `id`.
    pub fn data(&mut self, id: u32, data: &[u8]) {
        let Some(filter) = self.filters.get_mut(&id) else {
            return;
        };
        let filtered = filter.feed(data);
        if !filtered.reply.is_empty() {
            vlog!(
                1,
                "agent channel {id}: a request other than listing and signing was refused"
            );
        }
        for chunk in filtered.reply.chunks(MAX_DATA_LEN) {
            let _ = self.out.send(ClientFrame::Agent(id, chunk.to_vec()));
        }
        let taken = filtered.forward.is_empty() || self.channels.send(id, filtered.forward);
        if filtered.broken || !taken {
            self.close(id);
            let _ = self.out.send(ClientFrame::AgentClose(id));
        }
    }

    /// The server closed channel `id`.
    pub fn close(&mut self, id: u32) {
        self.filters.remove(&id);
        self.channels.close(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(kind: u8, body: &[u8]) -> Vec<u8> {
        let mut m = ((body.len() + 1) as u32).to_be_bytes().to_vec();
        m.push(kind);
        m.extend_from_slice(body);
        m
    }

    #[test]
    fn listing_and_signing_pass_and_the_rest_fails() {
        let mut filter = Filter::default();
        let mut input = message(REQUEST_IDENTITIES, b"");
        input.extend(message(SIGN_REQUEST, b"key and data"));
        input.extend(message(17, b"add identity"));
        input.extend(message(22, b"lock"));
        let got = filter.feed(&input);
        let mut allowed = message(REQUEST_IDENTITIES, b"");
        allowed.extend(message(SIGN_REQUEST, b"key and data"));
        assert_eq!(got.forward, allowed);
        assert_eq!(got.reply, [FAILURE, FAILURE].concat());
        assert!(!got.broken);
    }

    #[test]
    fn a_message_in_pieces_is_passed_when_complete() {
        let mut filter = Filter::default();
        let whole = message(SIGN_REQUEST, b"0123456789");
        for byte in &whole[..whole.len() - 1] {
            assert_eq!(filter.feed(&[*byte]), Filtered::default());
        }
        assert_eq!(filter.feed(&whole[whole.len() - 1..]).forward, whole);
    }

    #[test]
    fn bad_lengths_break_the_channel() {
        assert!(Filter::default().feed(&[0, 0, 0, 0]).broken);
        assert!(Filter::default().feed(&[0, 4, 0, 1]).broken);
    }
}
