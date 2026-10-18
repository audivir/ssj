//! Follows the output stream of the server just far enough to know when it is safe to write.
//!
//! The client has no copy of the screen and does not change or drop output. This tracker only
//! reads: it knows whether the last byte lies inside an escape sequence or a multi-byte
//! character, because a status message written there would corrupt the sequence. It also
//! remembers the last window title that the remote side set, so the title can be restored.

/// Longest title that is remembered.
const MAX_TITLE: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Ground,
    /// After ESC.
    Escape,
    /// Inside CSI, after `ESC [`.
    Csi,
    /// Inside OSC, after `ESC ]`. BEL ends it as well as ST.
    Osc,
    /// Inside DCS, SOS, PM, or APC. Only ST ends it.
    String,
    /// After ESC inside OSC or a string, where `\` completes the terminator.
    StringEscape {
        osc: bool,
    },
}

#[derive(Debug)]
pub struct Tracker {
    state: State,
    /// Bytes of a multi-byte character still to come.
    utf8_pending: u8,
    payload: Vec<u8>,
    payload_overflow: bool,
    title: Option<String>,
    /// A sequence was cancelled with CAN on the terminal; its rest is dropped when it arrives.
    skipping: bool,
}

impl Default for Tracker {
    fn default() -> Self {
        Self::new()
    }
}

impl Tracker {
    pub fn new() -> Self {
        Self {
            state: State::Ground,
            utf8_pending: 0,
            payload: Vec::new(),
            payload_overflow: false,
            title: None,
            skipping: false,
        }
    }

    /// Whether writing now would split a sequence or a character.
    pub fn in_sequence(&self) -> bool {
        !self.skipping && self.open()
    }

    /// Whether the state machine is inside a sequence or a character.
    fn open(&self) -> bool {
        self.state != State::Ground || self.utf8_pending > 0
    }

    /// Forgets a sequence that is still open, for a place where the stream was cut and the rest
    /// of the sequence will never come. The remembered title stays.
    pub fn abandon_sequence(&mut self) {
        self.state = State::Ground;
        self.utf8_pending = 0;
        self.payload.clear();
        self.payload_overflow = false;
        self.skipping = false;
    }

    /// The terminal was given a CAN in the middle of a sequence: its rest is dropped from the
    /// output with [`Self::skip_prefix`], so that it does not appear as text.
    pub fn cancel_sequence(&mut self) {
        if self.open() {
            self.skipping = true;
            // a half title must not be taken for the title
            self.payload.clear();
            self.payload_overflow = true;
        }
    }

    /// How many bytes at the start of `bytes` are the rest of a cancelled sequence.
    pub fn skip_prefix(&mut self, bytes: &[u8]) -> usize {
        if !self.skipping {
            return 0;
        }
        for (index, &byte) in bytes.iter().enumerate() {
            self.step(byte);
            if !self.open() {
                self.skipping = false;
                return index + 1;
            }
        }
        bytes.len()
    }

    /// The last title set with OSC 0 or OSC 2.
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.step(byte);
        }
    }

    fn step(&mut self, byte: u8) {
        // CAN and SUB cancel any sequence, in every state
        if matches!(byte, 0x18 | 0x1a) {
            self.finish_payload(false);
            self.state = State::Ground;
            self.utf8_pending = 0;
            return;
        }
        match self.state {
            State::Ground => self.ground(byte),
            State::Escape => match byte {
                b'[' => self.state = State::Csi,
                b']' => {
                    self.state = State::Osc;
                    self.payload.clear();
                    self.payload_overflow = false;
                }
                b'P' | b'X' | b'^' | b'_' => self.state = State::String,
                // intermediate bytes, such as in `ESC ( B`
                0x20..=0x2f => {}
                0x1b => {}
                _ => self.state = State::Ground,
            },
            State::Csi => match byte {
                0x40..=0x7e => self.state = State::Ground,
                0x1b => self.state = State::Escape,
                _ => {}
            },
            State::Osc => match byte {
                0x07 => {
                    self.finish_payload(true);
                    self.state = State::Ground;
                }
                0x1b => self.state = State::StringEscape { osc: true },
                _ => self.collect(byte),
            },
            State::String => {
                if byte == 0x1b {
                    self.state = State::StringEscape { osc: false };
                }
            }
            State::StringEscape { osc } => match byte {
                b'\\' => {
                    self.finish_payload(osc);
                    self.state = State::Ground;
                }
                // a new sequence starts and the old one is abandoned
                _ => {
                    self.finish_payload(false);
                    self.state = State::Escape;
                    self.step(byte);
                }
            },
        }
    }

    fn ground(&mut self, byte: u8) {
        if self.utf8_pending > 0 {
            if byte & 0xc0 == 0x80 {
                self.utf8_pending -= 1;
                return;
            }
            // a broken character: the byte starts something new
            self.utf8_pending = 0;
        }
        match byte {
            0x1b => self.state = State::Escape,
            0xc2..=0xdf => self.utf8_pending = 1,
            0xe0..=0xef => self.utf8_pending = 2,
            0xf0..=0xf4 => self.utf8_pending = 3,
            _ => {}
        }
    }

    fn collect(&mut self, byte: u8) {
        if self.payload.len() < MAX_TITLE + 2 {
            self.payload.push(byte);
        } else {
            self.payload_overflow = true;
        }
    }

    /// Reads a title from the payload of an OSC that ended properly.
    fn finish_payload(&mut self, complete: bool) {
        let payload = std::mem::take(&mut self.payload);
        let overflow = std::mem::take(&mut self.payload_overflow);
        if !complete || overflow {
            return;
        }
        let text = String::from_utf8_lossy(&payload);
        if let Some(title) = text.strip_prefix("0;").or_else(|| text.strip_prefix("2;")) {
            let title: String = title.chars().filter(|c| !c.is_control()).collect();
            self.title = Some(title);
        }
    }
}
