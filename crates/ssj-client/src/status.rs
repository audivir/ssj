//! Messages about the state of the connection.
//!
//! The client has no copy of the screen, so it must not draw over the application. On a terminal
//! the message goes to the window title. The old title is restored in two ways, because not every
//! terminal has the title stack of xterm: the stack is popped, and the title that the remote side
//! set last (known from the output) is set explicitly.

/// Shows and clears a message. The returned bytes belong into the output stream of the terminal.
pub trait Status: Send {
    fn show(&mut self, text: &str) -> Vec<u8>;
    /// `title` is the last title the remote side set, if it set one.
    fn clear(&mut self, title: Option<&str>) -> Vec<u8>;
}

/// Status that shows its message in the title of the terminal.
#[derive(Debug, Default)]
pub struct TitleStatus {
    active: bool,
}

impl Status for TitleStatus {
    fn show(&mut self, text: &str) -> Vec<u8> {
        let mut out = Vec::new();
        if !self.active {
            out.extend_from_slice(b"\x1b[22;0t");
            self.active = true;
        }
        // the text comes from the client itself, but a control character would end the title early.
        let safe: String = text.chars().filter(|c| !c.is_control()).collect();
        out.extend_from_slice(format!("\x1b]2;{safe}\x07").as_bytes());
        out
    }

    fn clear(&mut self, title: Option<&str>) -> Vec<u8> {
        if !std::mem::take(&mut self.active) {
            return Vec::new();
        }
        let mut out = b"\x1b[23;0t".to_vec();
        if let Some(title) = title {
            out.extend_from_slice(format!("\x1b]2;{title}\x07").as_bytes());
        }
        out
    }
}

/// Writes lines to stderr, for use without a terminal.
#[derive(Debug, Default)]
pub struct LineStatus;

impl Status for LineStatus {
    fn show(&mut self, text: &str) -> Vec<u8> {
        eprintln!("ssj: {text}");
        Vec::new()
    }

    fn clear(&mut self, _title: Option<&str>) -> Vec<u8> {
        Vec::new()
    }
}

/// The text written into the output where the server had to forget `count` bytes while the
/// client was away.
///
/// The bytes before and after the gap do not belong together, so the text first cancels any
/// escape sequence the terminal may be in the middle of (CAN) and resets the colors and styles.
pub fn dropped_notice(count: u64) -> Vec<u8> {
    format!("\x18\x1b[0m\r\n[ssj: {count} bytes of output were dropped]\r\n").into_bytes()
}
