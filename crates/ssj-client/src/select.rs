//! A menu on the terminal for choosing one of a few lines: arrow keys or `j`/`k`, Enter to choose,
//! `q` or Ctrl-C to cancel. Drawn on stderr, so that stdout stays free.

use std::io::{Read, Write};

use crate::terminal::RawMode;

/// What a key does to the menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    /// Jump to the n-th line, counted from 1.
    Number(usize),
    Enter,
    Cancel,
}

/// The keys in the bytes of one read. A lone Escape cancels; other sequences are ignored.
pub fn parse_keys(bytes: &[u8]) -> Vec<Key> {
    let mut keys = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            0x1b => match bytes.get(at + 1..at + 3) {
                Some(b"[A" | b"OA") => {
                    keys.push(Key::Up);
                    at += 3;
                }
                Some(b"[B" | b"OB") => {
                    keys.push(Key::Down);
                    at += 3;
                }
                _ if at + 1 == bytes.len() => {
                    keys.push(Key::Cancel);
                    at += 1;
                }
                // another sequence: skip its introducer and what follows up to a final byte
                _ => {
                    at += 2;
                    while at < bytes.len() && !(0x40..=0x7e).contains(&bytes[at]) {
                        at += 1;
                    }
                    at += 1;
                }
            },
            b'k' | b'p' | 0x10 => {
                keys.push(Key::Up);
                at += 1;
            }
            b'j' | b'n' | 0x0e => {
                keys.push(Key::Down);
                at += 1;
            }
            b'\r' | b'\n' => {
                keys.push(Key::Enter);
                at += 1;
            }
            b'q' | 0x03 | 0x04 => {
                keys.push(Key::Cancel);
                at += 1;
            }
            digit @ b'1'..=b'9' => {
                keys.push(Key::Number(usize::from(digit - b'0')));
                at += 1;
            }
            _ => at += 1,
        }
    }
    keys
}

/// The state of the menu.
#[derive(Debug)]
pub struct Menu {
    items: Vec<String>,
    selected: usize,
}

/// How a key left the menu.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Open,
    Chosen(usize),
    Cancelled,
}

impl Menu {
    /// A menu with at least one line, the first one selected.
    pub fn new(items: Vec<String>) -> Self {
        assert!(!items.is_empty(), "a menu needs a line");
        Self { items, selected: 0 }
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn press(&mut self, key: Key) -> Outcome {
        match key {
            Key::Up => self.selected = self.selected.saturating_sub(1),
            Key::Down => self.selected = (self.selected + 1).min(self.items.len() - 1),
            Key::Number(n) if n <= self.items.len() => self.selected = n - 1,
            Key::Number(_) => {}
            Key::Enter => return Outcome::Chosen(self.selected),
            Key::Cancel => return Outcome::Cancelled,
        }
        Outcome::Open
    }

    /// The lines of the menu, with the selected one marked. Every line ends in `\r\n`.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for (at, item) in self.items.iter().enumerate() {
            if at == self.selected {
                out.push_str(&format!("\x1b[1m> {} {item}\x1b[0m\x1b[K\r\n", at + 1));
            } else {
                out.push_str(&format!("  {} {item}\x1b[K\r\n", at + 1));
            }
        }
        out
    }

    fn lines(&self) -> usize {
        self.items.len()
    }
}

/// Asks the user to choose among `items`; `None` means that they cancelled or there is no
/// terminal. Blocks, so call it from a thread that may.
pub fn choose(title: &str, items: Vec<String>) -> Option<usize> {
    let _raw = RawMode::enable().ok().flatten()?;
    let mut menu = Menu::new(items);
    let mut err = std::io::stderr();
    // no cursor over the menu
    let _ = write!(err, "{title}\r\n\x1b[?25l{}", menu.render());
    let _ = err.flush();
    let mut stdin = std::io::stdin();
    let mut buf = [0u8; 64];
    let outcome = loop {
        let n = match stdin.read(&mut buf) {
            Ok(0) | Err(_) => break Outcome::Cancelled,
            Ok(n) => n,
        };
        let mut done = None;
        for key in parse_keys(&buf[..n]) {
            match menu.press(key) {
                Outcome::Open => {}
                finished => {
                    done = Some(finished);
                    break;
                }
            }
        }
        if let Some(finished) = done {
            break finished;
        }
        // draw again over the old lines
        let _ = write!(err, "\x1b[{}A{}", menu.lines(), menu.render());
        let _ = err.flush();
    };
    // take the menu and the title away, and give the cursor back
    let _ = write!(err, "\x1b[{}A\x1b[1A\x1b[J\x1b[?25h", menu.lines());
    let _ = err.flush();
    match outcome {
        Outcome::Chosen(at) => Some(at),
        _ => None,
    }
}
