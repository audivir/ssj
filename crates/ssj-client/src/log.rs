//! Debug output for `-v`, `-vv`, and `-vvv`, in the manner of `ssh -v`.
//!
//! Level 1 tells the steps (the bootstrap, the transports tried, a lost connection, a change of
//! the network), level 2 adds the details behind them (commands, addresses, offsets, timings of
//! the handshake), and level 3 every frame that is sent or received, by kind and size. The lines
//! go to the standard error output, or to a file with `--log-file`, which keeps a full-screen
//! program on the terminal intact.

use std::fmt::Arguments;
use std::fs::{File, OpenOptions};
use std::io::{IsTerminal, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

static LEVEL: AtomicU8 = AtomicU8::new(0);
static START: OnceLock<Instant> = OnceLock::new();
static FILE: Mutex<Option<File>> = Mutex::new(None);
static TERMINAL_LINES: AtomicBool = AtomicBool::new(false);

/// Switches the output on. `level` 0 keeps it off.
pub fn init(level: u8, file: Option<&Path>) -> std::io::Result<()> {
    if let Some(path) = file {
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        *FILE.lock().expect("not poisoned") = Some(options.open(path)?);
    }
    let _ = START.set(Instant::now());
    LEVEL.store(level.min(3), Ordering::Relaxed);
    Ok(())
}

/// Whether lines of `level` are written.
pub fn enabled(level: u8) -> bool {
    LEVEL.load(Ordering::Relaxed) >= level
}

/// Writes one line. Use the `vlog!` macro, which skips the formatting when the level is off.
pub fn emit(level: u8, args: Arguments<'_>) {
    let elapsed = START
        .get()
        .map_or(0.0, |start| start.elapsed().as_secs_f64());
    let line = format!("[ssj {elapsed:7.3}s] {args}");
    let mut file = FILE.lock().expect("not poisoned");
    match file.as_mut() {
        Some(file) => {
            let _ = writeln!(file, "{line}");
        }
        None => {
            // the terminal may be in raw mode, where a line feed alone would not return the cursor
            let end = if std::io::stderr().is_terminal() {
                "\r\n"
            } else {
                "\n"
            };
            let _ = write!(std::io::stderr(), "{line}{end}");
            // the lines of level 3 are about every frame, and drawing again sends frames
            if level < 3 {
                TERMINAL_LINES.store(true, Ordering::Relaxed);
            }
        }
    }
}

/// Whether lines were written to the terminal since the last call. They land wherever the cursor
/// is, over the screen of the program in the session.
pub fn take_terminal_lines() -> bool {
    TERMINAL_LINES.swap(false, Ordering::Relaxed)
}

/// `vlog!(1, "connected over {name}")` writes the line when the level is 1 or more.
#[macro_export]
macro_rules! vlog {
    ($level:expr, $($arg:tt)*) => {
        if $crate::log::enabled($level) {
            $crate::log::emit($level, format_args!($($arg)*));
        }
    };
}
