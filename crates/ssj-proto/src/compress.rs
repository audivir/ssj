//! Compression of output and input frames.
//!
//! Each frame is compressed on its own (zstd, level 3, no dictionary and no state shared with other
//! frames), so a frame can be sent or left out, and a connection can start anywhere, without the
//! two sides having to keep the same state. A frame is at most 32 KiB, which is enough for a good
//! ratio on the text that shells produce (about 21 percent of the original, against 26 percent for
//! deflate at level 1, and two to four times faster). A dictionary trained on other output did not
//! help: terminal traffic is too varied.
//!
//! The data comes from the other side of the connection, so the decoder is strict: the frame must
//! say how large its content is, the content may not be larger than a frame of data, and the
//! window that the decoder may allocate is small.

use zstd::bulk::{Compressor as ZstdCompressor, Decompressor as ZstdDecompressor};
use zstd::zstd_safe::{DParameter, get_frame_content_size};

use crate::frame::MAX_DATA_LEN;

/// The compressed form must save at least this fraction (1/8) of the bytes to be used.
const MIN_SAVING_DIVISOR: usize = 8;

/// Compression level: a better ratio than 1 for a sixth of the speed, and the link is what limits
/// the transfer whenever this runs.
const LEVEL: i32 = 3;

/// The largest window (as a power of two) that the decoder allocates: 128 KiB, which is more than
/// the window that a 32 KiB frame needs.
const WINDOW_LOG_MAX: u32 = 17;

/// Why a compressed frame was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InflateError {
    /// The data is not valid zstd, it is cut off, or it does not say how large its content is.
    Invalid,
    /// The content would be larger than a frame of output may be, which is how a bomb looks.
    TooLarge,
}

/// Compresses frames, reusing its state.
pub struct Compressor {
    state: ZstdCompressor<'static>,
}

impl Default for Compressor {
    fn default() -> Self {
        Self::new()
    }
}

impl Compressor {
    pub fn new() -> Self {
        Self {
            state: ZstdCompressor::new(LEVEL).expect("zstd accepts its own levels"),
        }
    }

    /// The compressed form of `data`, or `None` if it does not save an eighth of the bytes (data
    /// that is compressed already, or too short).
    pub fn compress(&mut self, data: &[u8]) -> Option<Vec<u8>> {
        if data.is_empty() || data.len() > MAX_DATA_LEN {
            return None;
        }
        let packed = self.state.compress(data).ok()?;
        (packed.len() <= data.len() - data.len() / MIN_SAVING_DIVISOR).then_some(packed)
    }
}

/// The data that `compressed` holds, at most [`MAX_DATA_LEN`] bytes of it.
pub fn inflate(compressed: &[u8]) -> Result<Vec<u8>, InflateError> {
    // the size is in the header, so a bomb is refused before anything is allocated for it.
    let size = match get_frame_content_size(compressed) {
        Ok(Some(size)) => size,
        _ => return Err(InflateError::Invalid),
    };
    if size == 0 {
        return Err(InflateError::Invalid);
    }
    if size > MAX_DATA_LEN as u64 {
        return Err(InflateError::TooLarge);
    }
    let mut state = ZstdDecompressor::new().map_err(|_| InflateError::Invalid)?;
    state
        .set_parameter(DParameter::WindowLogMax(WINDOW_LOG_MAX))
        .map_err(|_| InflateError::Invalid)?;
    let out = state
        .decompress(compressed, size as usize)
        .map_err(|_| InflateError::Invalid)?;
    // a frame that lies about its size is refused by zstd, but the length is part of the contract.
    if out.len() as u64 != size {
        return Err(InflateError::Invalid);
    }
    Ok(out)
}
