//! Small validated types of the protocol: window size, session id, and session name.

use serde::{Deserialize, Serialize};

/// Terminal dimensions in character cells, always between 1 and [`WindowSize::MAX`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "(u16, u16)", into = "(u16, u16)")]
pub struct WindowSize {
    rows: u16,
    cols: u16,
}

impl WindowSize {
    /// Largest number of rows or columns.
    pub const MAX: u16 = 4096;

    /// Returns `None` if a dimension is zero or larger than [`WindowSize::MAX`].
    pub fn new(rows: u16, cols: u16) -> Option<Self> {
        let valid = 1..=Self::MAX;
        (valid.contains(&rows) && valid.contains(&cols)).then_some(Self { rows, cols })
    }

    pub fn rows(self) -> u16 {
        self.rows
    }

    pub fn cols(self) -> u16 {
        self.cols
    }
}

impl TryFrom<(u16, u16)> for WindowSize {
    type Error = &'static str;

    fn try_from((rows, cols): (u16, u16)) -> Result<Self, &'static str> {
        Self::new(rows, cols).ok_or("a window size out of range")
    }
}

impl From<WindowSize> for (u16, u16) {
    fn from(size: WindowSize) -> Self {
        (size.rows, size.cols)
    }
}

/// Name that a user gives to a session, to find it again.
///
/// It appears in file names, commands, and lists, so only a few characters are allowed.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SessionName(String);

impl SessionName {
    /// Longest session name, in characters.
    pub const MAX_LEN: usize = 32;

    /// Accepts 1 to 32 characters of `A-Z a-z 0-9 . _ -`, not starting with `.` or `-`.
    pub fn parse(name: &str) -> Option<Self> {
        let valid = (1..=Self::MAX_LEN).contains(&name.len())
            && !name.starts_with(['.', '-'])
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
        valid.then(|| Self(name.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Whether `text` can name a session, as an id or as a name.
pub fn is_selector(text: &str) -> bool {
    SessionId::parse(text).is_some() || SessionName::parse(text).is_some()
}

/// Identifier of a session, restricted to characters that are safe in file names and commands.
///
/// It names control sockets and multiplexer sessions, so it must never contain path separators,
/// dots, whitespace, or option prefixes.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SessionId(String);

impl SessionId {
    const MIN_LEN: usize = 8;
    const MAX_LEN: usize = 64;

    /// Accepts lowercase ASCII letters and digits, between 8 and 64 characters.
    pub fn parse(id: &str) -> Option<Self> {
        let valid = (Self::MIN_LEN..=Self::MAX_LEN).contains(&id.len())
            && id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
        valid.then(|| Self(id.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}
