//! Speculative local echo: a typed character shows at once, dimmed, and the echo of the server
//! replaces it.
//!
//! The client has no copy of the screen, so the prediction has to be safe without one. It draws
//! only where it can be sure that the cursor sits at the end of a plain shell line, and it takes
//! its drawing back before every byte that the server sends, which then draws the real thing in
//! the same place. The rules:
//!
//! - Only printable ASCII is predicted. Any other key (arrows, control keys, UTF-8, backspace)
//!   stops the prediction until the next Enter.
//! - After Enter nothing is shown until the server has echoed one typed character (the epoch rule
//!   of mosh). A program that does not echo, such as a password prompt, never earns the trust.
//! - The output of the server must be the echo of what was typed, apart from colors, titles and a
//!   few mode switches. Any other character or sequence (a cursor movement, a full-screen
//!   program) drops the trust and every prediction.
//! - The column is estimated from the output, and nothing is drawn within a few columns of the
//!   right edge, where a wrap would scroll the screen and leave the drawing in the wrong place.
//! - A prediction that stays unanswered for [`EXPIRE`] is taken back.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// How long a prediction may wait for its echo.
pub const EXPIRE: Duration = Duration::from_secs(2);

/// Most characters that wait for an echo at the same time. A paste is far beyond it.
const MAX_PENDING: usize = 64;

/// A read of the terminal with more bytes than this is a paste, which is not predicted.
const PASTE: usize = 16;

/// Columns kept free at the right edge.
const EDGE: usize = 3;

/// The faint attribute, a character, and the attribute off again.
fn faint(byte: u8, out: &mut Vec<u8>) {
    out.extend_from_slice(b"\x1b[2m");
    out.push(byte);
    out.extend_from_slice(b"\x1b[22m");
}

#[derive(Clone, Copy, Debug)]
struct Entry {
    byte: u8,
    drawn: bool,
}

#[derive(Debug, Default)]
enum Parse {
    #[default]
    Ground,
    Esc,
    /// Inside `ESC [`, with the bytes between the bracket and the final byte.
    Csi(Vec<u8>),
    /// Inside `ESC ]`, up to BEL or `ESC \`.
    Osc,
    OscEsc,
}

#[derive(Debug)]
pub struct Echo {
    enabled: bool,
    /// The server has echoed a character since the last Enter.
    trusted: bool,
    /// Only printable characters were typed since the last Enter.
    plain: bool,
    pending: VecDeque<Entry>,
    /// Glyphs of predictions on the screen, to the right of where the server thinks the cursor is.
    drawn: usize,
    /// Column of the cursor according to the output, if it can be known.
    column: Option<usize>,
    columns: usize,
    since: Option<Instant>,
    parse: Parse,
}

impl Echo {
    pub fn new(enabled: bool, columns: u16) -> Self {
        Self {
            enabled,
            trusted: false,
            plain: true,
            pending: VecDeque::new(),
            drawn: 0,
            column: None,
            columns: usize::from(columns),
            since: None,
            parse: Parse::Ground,
        }
    }

    pub fn resize(&mut self, columns: u16) {
        self.columns = usize::from(columns);
        // the line may have been redrawn by the terminal
        self.column = None;
    }

    /// Bytes that take the drawn predictions off the screen.
    fn retract(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        if self.drawn > 0 {
            out.extend(std::iter::repeat_n(b'\x08', self.drawn));
            out.extend_from_slice(b"\x1b[K");
            self.drawn = 0;
        }
        self.since = None;
        out
    }

    /// Forgets what was predicted and takes it off the screen.
    pub fn cancel(&mut self) -> Vec<u8> {
        self.pending.clear();
        self.trusted = false;
        self.column = None;
        self.parse = Parse::Ground;
        self.retract()
    }

    /// When the oldest prediction runs out, if one waits.
    pub fn due(&self) -> Option<Instant> {
        self.since.map(|since| since + EXPIRE)
    }

    /// Takes back the predictions that waited too long. Returns the bytes for the screen.
    pub fn expire(&mut self, now: Instant) -> Vec<u8> {
        match self.since {
            Some(since) if now.duration_since(since) >= EXPIRE => self.cancel(),
            _ => Vec::new(),
        }
    }

    /// Typed bytes that go to the server. Returns what to draw now.
    pub fn typed(&mut self, bytes: &[u8], now: Instant) -> Vec<u8> {
        let mut out = Vec::new();
        if !self.enabled {
            return out;
        }
        if bytes.len() > PASTE {
            self.pending.clear();
            self.trusted = false;
            return out;
        }
        for &byte in bytes {
            match byte {
                0x20..=0x7e => {
                    let room = self
                        .column
                        .is_some_and(|column| column + self.drawn + 1 + EDGE < self.columns);
                    let all_drawn = self.pending.iter().all(|entry| entry.drawn);
                    let draw = self.trusted
                        && self.plain
                        && room
                        && all_drawn
                        && self.pending.len() < MAX_PENDING;
                    if self.pending.len() < MAX_PENDING {
                        self.pending.push_back(Entry { byte, drawn: draw });
                    } else {
                        // too much to follow: a paste, or a server that does not answer
                        self.pending.clear();
                        self.trusted = false;
                    }
                    if draw {
                        faint(byte, &mut out);
                        self.drawn += 1;
                        self.since.get_or_insert(now);
                    }
                }
                b'\r' | b'\n' => {
                    // a new line, and the first character of it is not shown
                    self.pending.clear();
                    self.trusted = false;
                    self.plain = true;
                }
                _ => {
                    self.pending.clear();
                    self.trusted = false;
                    self.plain = false;
                }
            }
        }
        out
    }

    /// Output of the server. Returns the bytes for the screen before it and after it.
    pub fn output(&mut self, data: &[u8], now: Instant) -> (Vec<u8>, Vec<u8>) {
        if !self.enabled {
            return (Vec::new(), Vec::new());
        }
        let before = self.retract();
        for &byte in data {
            self.feed(byte);
        }
        let mut after = Vec::new();
        for entry in &self.pending {
            if entry.drawn {
                faint(entry.byte, &mut after);
                self.drawn += 1;
            }
        }
        if self.drawn > 0 {
            self.since = Some(now);
        }
        (before, after)
    }

    /// Drops the trust and the predictions: something else than an echo came.
    fn mismatch(&mut self) {
        self.pending.clear();
        self.trusted = false;
    }

    fn printable(&mut self, byte: u8) {
        if let Some(column) = &mut self.column {
            *column += 1;
        }
        match self.pending.front() {
            Some(front) if front.byte == byte => {
                self.pending.pop_front();
                self.trusted = true;
            }
            Some(_) => self.mismatch(),
            None => {}
        }
    }

    fn feed(&mut self, byte: u8) {
        match std::mem::take(&mut self.parse) {
            Parse::Ground => match byte {
                0x1b => self.parse = Parse::Esc,
                b'\r' => {
                    self.column = Some(0);
                    if !self.pending.is_empty() {
                        self.mismatch();
                    }
                }
                b'\n' | 0x07 => {
                    if !self.pending.is_empty() {
                        self.mismatch();
                    }
                }
                0x20..=0x7e => self.printable(byte),
                // the start of a character of several bytes counts as one column
                0xc0..=0xff => {
                    if let Some(column) = &mut self.column {
                        *column += 1;
                    }
                    self.mismatch();
                }
                0x80..=0xbf => {}
                _ => {
                    // backspace, tab, and the rest move the cursor in ways that are not tracked
                    self.column = None;
                    self.mismatch();
                }
            },
            Parse::Esc => match byte {
                b'[' => self.parse = Parse::Csi(Vec::new()),
                b']' => self.parse = Parse::Osc,
                // keypad modes, which shells switch around the prompt
                b'=' | b'>' => {}
                _ => self.other_sequence(),
            },
            Parse::Csi(mut params) => match byte {
                0x40..=0x7e => self.csi(&params, byte),
                _ => {
                    if params.len() < 32 {
                        params.push(byte);
                        self.parse = Parse::Csi(params);
                    } else {
                        self.other_sequence();
                    }
                }
            },
            Parse::Osc => match byte {
                0x07 => {}
                0x1b => self.parse = Parse::OscEsc,
                _ => self.parse = Parse::Osc,
            },
            Parse::OscEsc => {
                if byte != b'\\' {
                    self.parse = Parse::Osc;
                }
            }
        }
    }

    fn csi(&mut self, params: &[u8], last: u8) {
        match (params, last) {
            // colors and attributes, erasing to the end of the line
            (_, b'm' | b'K') if params.iter().all(|b| b.is_ascii_digit() || *b == b';') => {}
            // application cursor keys and bracketed paste, which shells switch around the prompt
            (b"?1" | b"?2004", b'h' | b'l') => {}
            _ => self.other_sequence(),
        }
    }

    /// A sequence that moves the cursor or changes the screen: a full-screen program, or a
    /// shell that redraws the line.
    fn other_sequence(&mut self) {
        self.column = None;
        self.mismatch();
        self.plain = false;
    }
}
