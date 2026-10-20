//! The escape character of the client, as in `ssh`: `~` at the start of a line, and the detach key,
//! which works anywhere.

/// What the client has to do with the typed input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Bytes for the shell.
    Send(Vec<u8>),
    /// Leave the session and keep it running (`~.`).
    Detach,
    /// Show the list of escape sequences (`~?`).
    Help,
}

/// Filters typed input for escape sequences. Chunking of the input does not matter.
#[derive(Debug)]
pub struct EscapeFilter {
    /// `None` turns the feature off.
    escape: Option<u8>,
    /// A control byte that detaches wherever it is typed; `None` leaves every byte to the shell.
    detach_key: Option<u8>,
    at_line_start: bool,
    /// The escape character was typed at the start of a line and waits for the next byte.
    pending: bool,
}

impl EscapeFilter {
    pub fn new(escape: Option<u8>) -> Self {
        Self {
            escape,
            detach_key: None,
            at_line_start: true,
            pending: false,
        }
    }

    pub fn with_detach_key(mut self, key: Option<u8>) -> Self {
        self.detach_key = key;
        self
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Action> {
        if self.escape.is_none() && self.detach_key.is_none() {
            return vec![Action::Send(bytes.to_vec())];
        }
        let mut actions = Vec::new();
        let mut run = Vec::new();
        let flush = |run: &mut Vec<u8>, actions: &mut Vec<Action>| {
            if !run.is_empty() {
                actions.push(Action::Send(std::mem::take(run)));
            }
        };
        for &byte in bytes {
            if self.detach_key == Some(byte) {
                flush(&mut run, &mut actions);
                actions.push(Action::Detach);
                return actions;
            }
            // the escape character is off when it is none: no byte equals it.
            let Some(escape) = self.escape else {
                run.push(byte);
                continue;
            };
            if self.pending {
                self.pending = false;
                match byte {
                    b'.' => {
                        flush(&mut run, &mut actions);
                        actions.push(Action::Detach);
                        return actions;
                    }
                    b'?' => {
                        flush(&mut run, &mut actions);
                        actions.push(Action::Help);
                        self.at_line_start = false;
                    }
                    b if b == escape => {
                        run.push(escape);
                        self.at_line_start = false;
                    }
                    other => {
                        run.extend([escape, other]);
                        self.at_line_start = matches!(other, b'\r' | b'\n');
                    }
                }
            } else if self.at_line_start && byte == escape {
                self.pending = true;
                self.at_line_start = false;
            } else {
                run.push(byte);
                self.at_line_start = matches!(byte, b'\r' | b'\n');
            }
        }
        flush(&mut run, &mut actions);
        actions
    }
}

/// Text for `~?`; `detach_key` is added when one is set. Inside an `ssh` session, `ssh` reads
/// `~.` first, so the text says how to get it through.
pub fn help(detach_key: Option<u8>) -> String {
    help_in(detach_key, std::env::var_os("SSH_TTY").is_some())
}

/// Like [`help`], with the knowledge whether this runs inside an `ssh` session.
pub fn help_in(detach_key: Option<u8>, inside_ssh: bool) -> String {
    let mut text = String::from(
        "\r\nSupported escape sequences:\r\n  ~.  detach and leave the session running\r\n  ~~  send a single ~\r\n  ~?  this list\r\n(only at the start of a line)\r\n",
    );
    if let Some(key) = detach_key {
        // the key as it is typed: ^] for 0x1d.
        text.push_str(&format!(
            "  ^{}  detach, anywhere\r\n",
            char::from(key + b'@')
        ));
    }
    if inside_ssh {
        text.push_str(
            "(inside ssh: it reads ~. first and ends this connection, so ssj leaves the session\r\n running; type ~~. to detach cleanly, or set a detach key with --detach-key)\r\n",
        );
    }
    text
}
