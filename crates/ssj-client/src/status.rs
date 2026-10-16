//! Messages about the state of the connection.
//!
//! The client has no copy of the screen, so it must not draw over the application. On a terminal
//! the message goes to the window title, using the title stack of xterm to restore the old title.

/// Shows and clears a message. The returned bytes belong into the output stream of the terminal.
pub trait Status: Send {
    fn show(&mut self, text: &str) -> Vec<u8>;
    fn clear(&mut self) -> Vec<u8>;
}

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
        // the text comes from the client itself, but a control character would end the title early
        let safe: String = text.chars().filter(|c| !c.is_control()).collect();
        out.extend_from_slice(format!("\x1b]2;{safe}\x07").as_bytes());
        out
    }

    fn clear(&mut self) -> Vec<u8> {
        if std::mem::take(&mut self.active) {
            b"\x1b[23;0t".to_vec()
        } else {
            Vec::new()
        }
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

    fn clear(&mut self) -> Vec<u8> {
        Vec::new()
    }
}
