//! Frames as `length:u32 | body`, where the length is big endian and the body is the frame in the
//! format of `postcard`: the index of the variant, then its fields, integers as varints and byte
//! strings with their length in front.
//!
//! `length` counts the body. Decoding takes a slice and either returns a frame with the number of
//! consumed bytes, asks for more bytes, or fails. A header that announces an oversized frame fails
//! before any body is buffered.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::env::Env;
use crate::types::WindowSize;

/// Largest value of the length field.
pub const MAX_FRAME_LEN: usize = 128 * 1024;
/// Largest chunk of terminal data in one frame.
pub const MAX_DATA_LEN: usize = 32 * 1024;

/// Failure to decode a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// Header announces a length of zero or more than [`MAX_FRAME_LEN`].
    BadLength,
    /// Body is not a frame, has bytes after it, or holds a value outside its allowed range.
    Invalid,
}

/// Failure to encode a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncodeError {
    /// Data chunk is empty or larger than [`MAX_DATA_LEN`], or a gap is empty.
    BadValue,
}

/// Reason the server ends a session attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorCode {
    /// `Start` was sent but the shell exists already.
    AlreadyStarted,
    /// `Resume` or `Attach` was sent but no shell exists.
    NotStarted,
    /// The requested output offset is not in the replay buffer anymore.
    ReplayGap,
    /// The first frame of the client was not a valid `Hello`.
    BadHello,
    Internal,
    /// The client sent a frame at a wrong time or with a wrong value.
    Violation,
}

/// How the shell ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExitStatus {
    Code(u8),
    Signal(u8),
}

/// First frame of every connection of the client.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Hello {
    /// Creates the shell of a fresh session. With `agent` the shell gets a socket for the agent
    /// of the client (see [`ClientFrame::AgentOffer`]).
    Start {
        size: WindowSize,
        env: Env,
        agent: bool,
    },
    /// Continues after a lost connection. `output_offset` counts the output bytes already seen.
    Resume {
        size: WindowSize,
        output_offset: u64,
    },
    /// Joins a running session as a new client without history.
    Attach { size: WindowSize },
}

/// Answer of the server to a valid `Hello`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Welcome {
    /// Input bytes the server has consumed. The client resends everything after this offset.
    pub input_offset: u64,
    /// Offset of the first output byte that follows.
    pub output_offset: u64,
}

/// Frame that the client sends.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientFrame {
    Hello(Hello),
    Input(#[serde(with = "serde_bytes")] Vec<u8>),
    Resize(WindowSize),
    /// Total output bytes the client has processed.
    OutputAck(u64),
    /// The client is done, for example after it received the exit status.
    Close,
    /// The client leaves on purpose and the session keeps running without a timeout.
    Detach,
    /// The terminal of the client was closed, or `ssj` was told to end: the server hangs up the
    /// terminal of the shell, as `ssh` would, and the session ends. Losing the connection is not
    /// this, since it may come back.
    Hangup,
    /// Asks the server to answer with `Pong`, which shows that the path works.
    Ping,
    /// Input that is compressed (see [`crate::compress`]). It holds the same as an `Input` frame
    /// and counts for the input offset by its uncompressed length.
    Compressed(#[serde(with = "serde_bytes")] Vec<u8>),
    /// This connection serves the agent of the client: the server may open channels to it.
    AgentOffer,
    /// Bytes from the agent on channel `id`.
    Agent(u32, #[serde(with = "serde_bytes")] Vec<u8>),
    /// The agent closed channel `id`, or the client refused it.
    AgentClose(u32),
}

/// Frame that the server sends.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerFrame {
    /// The first variant, so that [`peek_server`] knows its index.
    Output(#[serde(with = "serde_bytes")] Vec<u8>),
    Welcome(Welcome),
    /// Total input bytes the server has consumed.
    InputAck(u64),
    Exit(ExitStatus),
    Error(ErrorCode),
    /// Answer to `Ping`.
    Pong,
    /// The server skipped this many bytes of output, because the client was too slow to take
    /// them (see `slow_client`). The stream goes on after them, and the client adds the number
    /// to its output offset.
    Gap(u64),
    /// Someone detached this client from the host (`ssj detach`): the client leaves and the
    /// session keeps running, as after the detach key.
    Detached,
    /// Output that is compressed (see [`crate::compress`]). It holds the same as an `Output`
    /// frame and counts for the output offset by its uncompressed length.
    Compressed(#[serde(with = "serde_bytes")] Vec<u8>),
    /// A program on the server connected to the agent socket. The server numbers the channels.
    AgentOpen(u32),
    /// Bytes from that program on channel `id`.
    Agent(u32, #[serde(with = "serde_bytes")] Vec<u8>),
    /// The program closed channel `id`.
    AgentClose(u32),
}

/// The index of [`ServerFrame::Output`], which is also the first byte of its body.
const OUTPUT: u8 = 0;

/// Whether `data` is a chunk that a frame may carry: not empty and at most [`MAX_DATA_LEN`].
fn chunk(data: &[u8]) -> bool {
    (1..=MAX_DATA_LEN).contains(&data.len())
}

/// Frame of either side, with the checks that its type alone does not make.
trait Frame: Serialize + DeserializeOwned {
    fn valid(&self) -> bool;
}

impl Frame for ClientFrame {
    fn valid(&self) -> bool {
        match self {
            Self::Input(data) | Self::Compressed(data) | Self::Agent(_, data) => chunk(data),
            _ => true,
        }
    }
}

impl Frame for ServerFrame {
    fn valid(&self) -> bool {
        match self {
            Self::Output(data) | Self::Compressed(data) | Self::Agent(_, data) => chunk(data),
            // a gap of nothing would be a frame that does nothing.
            Self::Gap(skipped) => *skipped > 0,
            _ => true,
        }
    }
}

/// Appends the encoded frame to `out`. On error `out` is left unchanged.
fn encode(frame: &impl Frame, out: &mut Vec<u8>) -> Result<(), EncodeError> {
    if !frame.valid() {
        return Err(EncodeError::BadValue);
    }
    let header_at = out.len();
    out.extend_from_slice(&[0; 4]);
    *out = postcard::to_extend(frame, std::mem::take(out)).expect("a frame fits in a vector");
    let len = (out.len() - header_at - 4) as u32;
    debug_assert!((1..=MAX_FRAME_LEN as u32).contains(&len));
    out[header_at..header_at + 4].copy_from_slice(&len.to_be_bytes());
    Ok(())
}

/// Splits the body of the first frame off `buf`: `Ok(None)` means more bytes are needed.
fn split_frame(buf: &[u8]) -> Result<Option<&[u8]>, DecodeError> {
    let Some(header) = buf.get(..4) else {
        return Ok(None);
    };
    let len = u32::from_be_bytes(header.try_into().unwrap()) as usize;
    if len == 0 || len > MAX_FRAME_LEN {
        return Err(DecodeError::BadLength);
    }
    Ok(buf.get(4..4 + len))
}

/// Decodes the first frame of `buf`, returning it with the number of bytes it used.
fn decode<F: Frame>(buf: &[u8]) -> Result<Option<(F, usize)>, DecodeError> {
    let Some(body) = split_frame(buf)? else {
        return Ok(None);
    };
    match postcard::take_from_bytes::<F>(body) {
        Ok((frame, [])) if frame.valid() => Ok(Some((frame, 4 + body.len()))),
        _ => Err(DecodeError::Invalid),
    }
}

impl ClientFrame {
    /// Appends the encoded frame to `out`. On error `out` is left unchanged.
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<(), EncodeError> {
        encode(self, out)
    }
}

impl ServerFrame {
    /// Appends the encoded frame to `out`. On error `out` is left unchanged.
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<(), EncodeError> {
        encode(self, out)
    }
}

/// Decodes the first frame of `buf`, returning it with the number of bytes it used.
pub fn decode_client(buf: &[u8]) -> Result<Option<(ClientFrame, usize)>, DecodeError> {
    decode(buf)
}

/// Decodes the first frame of `buf`, returning it with the number of bytes it used.
pub fn decode_server(buf: &[u8]) -> Result<Option<(ServerFrame, usize)>, DecodeError> {
    decode(buf)
}

/// What the first server frame in a buffer is, found without decoding it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Peek {
    /// The frame is not complete yet.
    Incomplete,
    /// A complete output frame: where its data lies in the buffer, and how many bytes the frame
    /// uses. Reading the data from the buffer saves the copy that decoding makes.
    Output {
        data: std::ops::Range<usize>,
        used: usize,
    },
    /// A complete frame of another kind, to be decoded with [`decode_server`].
    Other,
}

/// Looks at the first server frame of `buf` and reports what it is, without allocating.
pub fn peek_server(buf: &[u8]) -> Result<Peek, DecodeError> {
    let Some(body) = split_frame(buf)? else {
        return Ok(Peek::Incomplete);
    };
    let used = 4 + body.len();
    match body.split_first() {
        Some((&OUTPUT, rest)) => match postcard::take_from_bytes::<&[u8]>(rest) {
            // the data ends the frame.
            Ok((data, [])) if chunk(data) => Ok(Peek::Output {
                data: used - data.len()..used,
                used,
            }),
            _ => Err(DecodeError::Invalid),
        },
        _ => Ok(Peek::Other),
    }
}
