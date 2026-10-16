//! Frames as `length:u32 | type:u8 | body`, all integers big endian.
//!
//! `length` counts the type byte and the body. Decoding takes a slice and either returns a frame
//! with the number of consumed bytes, asks for more bytes, or fails. A header that announces an
//! oversized frame fails before any body is buffered.

use crate::env::{Env, EnvError};
use crate::types::WindowSize;

/// Largest value of the length field.
pub const MAX_FRAME_LEN: usize = 128 * 1024;
/// Largest chunk of terminal data in one frame.
pub const MAX_DATA_LEN: usize = 32 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// Header announces a length of zero or more than [`MAX_FRAME_LEN`].
    BadLength,
    UnknownType(u8),
    /// Body ends before all fields are read.
    Truncated,
    /// Body has bytes after the last field.
    TrailingBytes,
    /// Field holds a value outside its allowed range.
    InvalidValue,
    Env(EnvError),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncodeError {
    /// Data chunk is empty or larger than [`MAX_DATA_LEN`].
    BadDataLength,
}

/// Reason the server ends a session attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorCode {
    /// `Start` was sent but the shell exists already.
    AlreadyStarted = 1,
    /// `Resume` or `Attach` was sent but no shell exists.
    NotStarted = 2,
    /// The requested output offset is not in the replay buffer anymore.
    ReplayGap = 3,
    /// The first frame of the client was not a valid `Hello`.
    BadHello = 4,
    Internal = 5,
    /// The client sent a frame at a wrong time or with a wrong value.
    Violation = 6,
}

impl ErrorCode {
    fn from_u8(value: u8) -> Option<Self> {
        Some(match value {
            1 => Self::AlreadyStarted,
            2 => Self::NotStarted,
            3 => Self::ReplayGap,
            4 => Self::BadHello,
            5 => Self::Internal,
            6 => Self::Violation,
            _ => return None,
        })
    }
}

/// How the shell ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitStatus {
    Code(u8),
    Signal(u8),
}

/// First frame of every connection of the client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Hello {
    /// Creates the shell of a fresh session.
    Start { size: WindowSize, env: Env },
    /// Continues after a lost connection. `output_offset` counts the output bytes already seen.
    Resume {
        size: WindowSize,
        output_offset: u64,
    },
    /// Joins a running session as a new client without history.
    Attach { size: WindowSize },
}

/// Answer of the server to a valid `Hello`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Welcome {
    /// Input bytes the server has consumed. The client resends everything after this offset.
    pub input_offset: u64,
    /// Offset of the first output byte that follows.
    pub output_offset: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientFrame {
    Hello(Hello),
    Input(Vec<u8>),
    Resize(WindowSize),
    /// Total output bytes the client has processed.
    OutputAck(u64),
    /// The client is done, for example after it received the exit status.
    Close,
    /// The client leaves on purpose and the session keeps running without a timeout.
    Detach,
    /// Asks the server to answer with `Pong`, which shows that the path works.
    Ping,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServerFrame {
    Welcome(Welcome),
    Output(Vec<u8>),
    /// Total input bytes the server has consumed.
    InputAck(u64),
    Exit(ExitStatus),
    Error(ErrorCode),
    /// Answer to `Ping`.
    Pong,
}

const HELLO: u8 = 0x01;
const INPUT: u8 = 0x02;
const RESIZE: u8 = 0x03;
const OUTPUT_ACK: u8 = 0x04;
const CLOSE: u8 = 0x05;
const DETACH: u8 = 0x06;
const PING: u8 = 0x07;

const WELCOME: u8 = 0x81;
const OUTPUT: u8 = 0x82;
const INPUT_ACK: u8 = 0x83;
const EXIT: u8 = 0x84;
const ERROR: u8 = 0x85;
const PONG: u8 = 0x86;

const HELLO_START: u8 = 0;
const HELLO_RESUME: u8 = 1;
const HELLO_ATTACH: u8 = 2;

const EXIT_CODE: u8 = 0;
const EXIT_SIGNAL: u8 = 1;

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        if self.0.len() < n {
            return Err(DecodeError::Truncated);
        }
        let (head, tail) = self.0.split_at(n);
        self.0 = tail;
        Ok(head)
    }

    fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, DecodeError> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }

    fn u64(&mut self) -> Result<u64, DecodeError> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn size(&mut self) -> Result<WindowSize, DecodeError> {
        let (rows, cols) = (self.u16()?, self.u16()?);
        WindowSize::new(rows, cols).ok_or(DecodeError::InvalidValue)
    }

    fn string(&mut self) -> Result<String, DecodeError> {
        let len = usize::from(self.u16()?);
        let bytes = self.take(len)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| DecodeError::InvalidValue)
    }

    /// Takes the rest of the body as a non-empty data chunk.
    fn data(&mut self) -> Result<Vec<u8>, DecodeError> {
        let rest = std::mem::take(&mut self.0);
        if rest.is_empty() || rest.len() > MAX_DATA_LEN {
            return Err(DecodeError::InvalidValue);
        }
        Ok(rest.to_vec())
    }

    fn finish(self) -> Result<(), DecodeError> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(DecodeError::TrailingBytes)
        }
    }
}

fn put_size(out: &mut Vec<u8>, size: WindowSize) {
    out.extend_from_slice(&size.rows().to_be_bytes());
    out.extend_from_slice(&size.cols().to_be_bytes());
}

fn put_string(out: &mut Vec<u8>, s: &str) {
    // `Env` limits keep every string below `u16::MAX`
    out.extend_from_slice(&(s.len() as u16).to_be_bytes());
    out.extend_from_slice(s.as_bytes());
}

fn put_data(out: &mut Vec<u8>, tag: u8, data: &[u8]) -> Result<(), EncodeError> {
    if data.is_empty() || data.len() > MAX_DATA_LEN {
        return Err(EncodeError::BadDataLength);
    }
    out.push(tag);
    out.extend_from_slice(data);
    Ok(())
}

/// Appends the frame to `out` with its length header.
fn finish_frame(out: &mut [u8], header_at: usize) {
    let len = (out.len() - header_at - 4) as u32;
    debug_assert!((1..=MAX_FRAME_LEN as u32).contains(&len));
    out[header_at..header_at + 4].copy_from_slice(&len.to_be_bytes());
}

impl ClientFrame {
    /// Appends the encoded frame to `out`. On error `out` is left unchanged.
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<(), EncodeError> {
        let header_at = out.len();
        out.extend_from_slice(&[0; 4]);
        let result = match self {
            Self::Hello(hello) => {
                out.push(HELLO);
                match hello {
                    Hello::Start { size, env } => {
                        out.push(HELLO_START);
                        put_size(out, *size);
                        out.extend_from_slice(&(env.len() as u16).to_be_bytes());
                        for (name, value) in env.iter() {
                            put_string(out, name);
                            put_string(out, value);
                        }
                    }
                    Hello::Resume {
                        size,
                        output_offset,
                    } => {
                        out.push(HELLO_RESUME);
                        put_size(out, *size);
                        out.extend_from_slice(&output_offset.to_be_bytes());
                    }
                    Hello::Attach { size } => {
                        out.push(HELLO_ATTACH);
                        put_size(out, *size);
                    }
                }
                Ok(())
            }
            Self::Input(data) => put_data(out, INPUT, data),
            Self::Resize(size) => {
                out.push(RESIZE);
                put_size(out, *size);
                Ok(())
            }
            Self::OutputAck(offset) => {
                out.push(OUTPUT_ACK);
                out.extend_from_slice(&offset.to_be_bytes());
                Ok(())
            }
            Self::Close => {
                out.push(CLOSE);
                Ok(())
            }
            Self::Detach => {
                out.push(DETACH);
                Ok(())
            }
            Self::Ping => {
                out.push(PING);
                Ok(())
            }
        };
        match result {
            Ok(()) => {
                finish_frame(out, header_at);
                Ok(())
            }
            Err(err) => {
                out.truncate(header_at);
                Err(err)
            }
        }
    }

    fn decode_body(tag: u8, mut body: Reader<'_>) -> Result<Self, DecodeError> {
        let frame = match tag {
            HELLO => Self::Hello(match body.u8()? {
                HELLO_START => {
                    let size = body.size()?;
                    let count = usize::from(body.u16()?);
                    // a count above the limit is rejected before any allocation
                    if count > Env::MAX_VARS {
                        return Err(DecodeError::Env(EnvError::TooMany));
                    }
                    let mut vars = Vec::with_capacity(count);
                    for _ in 0..count {
                        vars.push((body.string()?, body.string()?));
                    }
                    let env = Env::new(vars).map_err(DecodeError::Env)?;
                    Hello::Start { size, env }
                }
                HELLO_RESUME => Hello::Resume {
                    size: body.size()?,
                    output_offset: body.u64()?,
                },
                HELLO_ATTACH => Hello::Attach { size: body.size()? },
                _ => return Err(DecodeError::InvalidValue),
            }),
            INPUT => Self::Input(body.data()?),
            RESIZE => Self::Resize(body.size()?),
            OUTPUT_ACK => Self::OutputAck(body.u64()?),
            CLOSE => Self::Close,
            DETACH => Self::Detach,
            PING => Self::Ping,
            other => return Err(DecodeError::UnknownType(other)),
        };
        body.finish()?;
        Ok(frame)
    }
}

impl ServerFrame {
    /// Appends the encoded frame to `out`. On error `out` is left unchanged.
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<(), EncodeError> {
        let header_at = out.len();
        out.extend_from_slice(&[0; 4]);
        let result = match self {
            Self::Welcome(welcome) => {
                out.push(WELCOME);
                out.extend_from_slice(&welcome.input_offset.to_be_bytes());
                out.extend_from_slice(&welcome.output_offset.to_be_bytes());
                Ok(())
            }
            Self::Output(data) => put_data(out, OUTPUT, data),
            Self::InputAck(offset) => {
                out.push(INPUT_ACK);
                out.extend_from_slice(&offset.to_be_bytes());
                Ok(())
            }
            Self::Exit(status) => {
                out.push(EXIT);
                match status {
                    ExitStatus::Code(code) => out.extend_from_slice(&[EXIT_CODE, *code]),
                    ExitStatus::Signal(signal) => out.extend_from_slice(&[EXIT_SIGNAL, *signal]),
                }
                Ok(())
            }
            Self::Error(code) => {
                out.push(ERROR);
                out.push(*code as u8);
                Ok(())
            }
            Self::Pong => {
                out.push(PONG);
                Ok(())
            }
        };
        match result {
            Ok(()) => {
                finish_frame(out, header_at);
                Ok(())
            }
            Err(err) => {
                out.truncate(header_at);
                Err(err)
            }
        }
    }

    fn decode_body(tag: u8, mut body: Reader<'_>) -> Result<Self, DecodeError> {
        let frame = match tag {
            WELCOME => Self::Welcome(Welcome {
                input_offset: body.u64()?,
                output_offset: body.u64()?,
            }),
            OUTPUT => Self::Output(body.data()?),
            INPUT_ACK => Self::InputAck(body.u64()?),
            EXIT => {
                let (kind, value) = (body.u8()?, body.u8()?);
                Self::Exit(match kind {
                    EXIT_CODE => ExitStatus::Code(value),
                    EXIT_SIGNAL => ExitStatus::Signal(value),
                    _ => return Err(DecodeError::InvalidValue),
                })
            }
            ERROR => Self::Error(ErrorCode::from_u8(body.u8()?).ok_or(DecodeError::InvalidValue)?),
            PONG => Self::Pong,
            other => return Err(DecodeError::UnknownType(other)),
        };
        body.finish()?;
        Ok(frame)
    }
}

/// Splits the first frame off `buf`: `Ok(None)` means more bytes are needed.
fn split_frame(buf: &[u8]) -> Result<Option<(u8, Reader<'_>, usize)>, DecodeError> {
    let Some(header) = buf.get(..4) else {
        return Ok(None);
    };
    let len = u32::from_be_bytes(header.try_into().unwrap()) as usize;
    if len == 0 || len > MAX_FRAME_LEN {
        return Err(DecodeError::BadLength);
    }
    let Some(frame) = buf.get(4..4 + len) else {
        return Ok(None);
    };
    Ok(Some((frame[0], Reader(&frame[1..]), 4 + len)))
}

/// Decodes the first frame of `buf`, returning it with the number of bytes it used.
pub fn decode_client(buf: &[u8]) -> Result<Option<(ClientFrame, usize)>, DecodeError> {
    match split_frame(buf)? {
        Some((tag, body, used)) => Ok(Some((ClientFrame::decode_body(tag, body)?, used))),
        None => Ok(None),
    }
}

/// Decodes the first frame of `buf`, returning it with the number of bytes it used.
pub fn decode_server(buf: &[u8]) -> Result<Option<(ServerFrame, usize)>, DecodeError> {
    match split_frame(buf)? {
        Some((tag, body, used)) => Ok(Some((ServerFrame::decode_body(tag, body)?, used))),
        None => Ok(None),
    }
}
