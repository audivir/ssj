use std::collections::VecDeque;

/// Bytes sent but not yet acknowledged, addressed by their absolute offset in the stream.
///
/// The capacity is fixed, which keeps the memory bounded. When the buffer is full, the writer
/// either waits until the peer acknowledges data ([`ReplayBuffer::push`]), or forgets the oldest
/// bytes ([`ReplayBuffer::push_evicting`]) when nobody is there to acknowledge them.
#[derive(Debug)]
pub struct ReplayBuffer {
    data: VecDeque<u8>,
    /// Offset of the first byte in `data`.
    start: u64,
    capacity: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplayError {
    /// The requested offset was acknowledged and dropped already.
    Gone,
    /// The requested offset is beyond the last byte written.
    Ahead,
}

impl ReplayBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            data: VecDeque::new(),
            start: 0,
            capacity,
        }
    }

    /// Empty buffer whose stream already has `offset` bytes behind it.
    pub fn starting_at(capacity: usize, offset: u64) -> Self {
        Self {
            data: VecDeque::new(),
            start: offset,
            capacity,
        }
    }

    /// Offset of the oldest retained byte.
    pub fn start(&self) -> u64 {
        self.start
    }

    /// Offset after the newest byte, which is the total number of bytes ever written.
    pub fn end(&self) -> u64 {
        self.start + self.data.len() as u64
    }

    pub fn free(&self) -> usize {
        self.capacity - self.data.len()
    }

    /// Stores as many bytes of `bytes` as fit and returns how many were stored.
    pub fn push(&mut self, bytes: &[u8]) -> usize {
        let n = bytes.len().min(self.free());
        self.data.extend(&bytes[..n]);
        n
    }

    /// Stores all of `bytes` and forgets the oldest bytes to make room. Returns how many bytes
    /// the start of the buffer moved, which is the number of bytes that were lost.
    ///
    /// If `bytes` alone is larger than the capacity, only its last bytes are kept.
    pub fn push_evicting(&mut self, bytes: &[u8]) -> u64 {
        let old_start = self.start;
        let new_end = self.end() + bytes.len() as u64;
        let keep_new = bytes.len().min(self.capacity);
        let keep_old = (self.capacity - keep_new).min(self.data.len());
        self.data.drain(..self.data.len() - keep_old);
        self.data.extend(&bytes[bytes.len() - keep_new..]);
        self.start = new_end - self.data.len() as u64;
        self.start - old_start
    }

    /// Drops all bytes before `offset`. Stale acknowledgements are ignored.
    ///
    /// Fails if `offset` is beyond the last byte written, which only a faulty or hostile peer sends.
    pub fn ack(&mut self, offset: u64) -> Result<(), ReplayError> {
        if offset > self.end() {
            return Err(ReplayError::Ahead);
        }
        if offset > self.start {
            self.data.drain(..(offset - self.start) as usize);
            self.start = offset;
        }
        Ok(())
    }

    /// Copies up to `max` bytes starting at `offset`.
    pub fn read_from(&self, offset: u64, max: usize) -> Result<Vec<u8>, ReplayError> {
        if offset < self.start {
            return Err(ReplayError::Gone);
        }
        if offset > self.end() {
            return Err(ReplayError::Ahead);
        }
        let skip = (offset - self.start) as usize;
        Ok(self.data.range(skip..).take(max).copied().collect())
    }

    /// Offset from which at most `tail` of the newest bytes are still available.
    pub fn tail_offset(&self, tail: usize) -> u64 {
        self.end().saturating_sub(tail as u64).max(self.start)
    }
}
