//! Raw mode and window size of the local terminal.

use std::io;

use rustix::stdio::stdin;
use rustix::termios::{OptionalActions, Termios, isatty, tcgetattr, tcgetwinsize, tcsetattr};
use ssj_proto::WindowSize;

/// Puts the terminal into raw mode and restores it when dropped.
pub struct RawMode {
    original: Termios,
}

impl RawMode {
    /// Returns `None` if stdin is not a terminal, such as in a pipe.
    pub fn enable() -> io::Result<Option<Self>> {
        let fd = stdin();
        if !isatty(fd) {
            return Ok(None);
        }
        let original = tcgetattr(fd)?;
        let mut raw = original.clone();
        raw.make_raw();
        tcsetattr(fd, OptionalActions::Now, &raw)?;
        Ok(Some(Self { original }))
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = tcsetattr(stdin(), OptionalActions::Now, &self.original);
    }
}

/// Size of the terminal, or 24 by 80 if it cannot be read.
pub fn window_size() -> WindowSize {
    tcgetwinsize(stdin())
        .ok()
        .and_then(|size| WindowSize::new(size.ws_row, size.ws_col))
        .unwrap_or_else(|| WindowSize::new(24, 80).expect("valid size"))
}
