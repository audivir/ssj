//! Collects a burst of window sizes into the last one.

use std::time::Duration;

use ssj_proto::WindowSize;
use tokio::time::Instant;

/// Dragging a window edge makes dozens of sizes per second. Only the size at rest matters, so one
/// is sent when no new one has come for the quiet time (trailing edge).
#[derive(Debug)]
pub struct Debounce {
    quiet: Duration,
    latest: Option<(WindowSize, Instant)>,
}

impl Debounce {
    pub fn new(quiet: Duration) -> Self {
        Self {
            quiet,
            latest: None,
        }
    }

    pub fn push(&mut self, size: WindowSize, now: Instant) {
        self.latest = Some((size, now + self.quiet));
    }

    /// When the waiting size is due, if there is one.
    pub fn due(&self) -> Option<Instant> {
        self.latest.map(|(_, due)| due)
    }

    /// The waiting size once it is due.
    pub fn take(&mut self, now: Instant) -> Option<WindowSize> {
        match self.latest {
            Some((size, due)) if now >= due => {
                self.latest = None;
                Some(size)
            }
            _ => None,
        }
    }
}
