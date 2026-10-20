//! Notices that the network changed or that the machine woke up from sleep.
//!
//! Every source sends a [`Cause`] on one channel. The client reacts to a cause by moving its UDP
//! socket and checking the path at once, which is much faster than waiting for a timeout. A
//! source that is missing or fails only makes the reaction slower, never wrong.
//!
//! Sources:
//! - the clock watch, on every system: a timer that fires much later than it should means that
//!   the process was suspended;
//! - macOS: changes of the network state in the dynamic store of SystemConfiguration;
//! - Linux: address, link, and route changes reported through a netlink socket.

use std::time::{Duration, Instant, SystemTime};

use tokio::sync::mpsc;
use tokio::time::{interval, timeout_at};

/// Reason why the watcher reports a change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Cause {
    NetworkChanged,
    /// The machine was suspended. It ranks higher, since the old path is likely dead.
    Woke,
}

/// Compares the time that passed on two clocks with the time a timer should have taken.
///
/// During sleep one of the clocks stops on some systems and both run on others, so a late timer
/// is judged on whichever clock saw more time pass.
#[derive(Debug)]
pub struct ClockWatch {
    wall: SystemTime,
    mono: Instant,
    expected: Duration,
    threshold: Duration,
}

impl ClockWatch {
    pub fn new(wall: SystemTime, mono: Instant, expected: Duration, threshold: Duration) -> Self {
        Self {
            wall,
            mono,
            expected,
            threshold,
        }
    }

    /// Records a tick and says whether it came so late that the process must have been suspended.
    pub fn tick(&mut self, wall: SystemTime, mono: Instant) -> bool {
        let wall_passed = wall.duration_since(self.wall).unwrap_or_default();
        let mono_passed = mono.saturating_duration_since(self.mono);
        self.wall = wall;
        self.mono = mono;
        wall_passed.max(mono_passed) > self.expected + self.threshold
    }
}

const CLOCK_TICK: Duration = Duration::from_secs(1);
const CLOCK_THRESHOLD: Duration = Duration::from_secs(3);

/// Sources that were started, by name.
pub struct Sources {
    pub events: mpsc::UnboundedReceiver<Cause>,
    pub active: Vec<&'static str>,
}

/// Starts all sources that exist on this system. Must run inside the tokio runtime.
pub fn start() -> Sources {
    let (tx, events) = mpsc::unbounded_channel();
    let mut active = Vec::new();

    let clock_tx = tx.clone();
    tokio::spawn(async move {
        let mut watch = ClockWatch::new(
            SystemTime::now(),
            Instant::now(),
            CLOCK_TICK,
            CLOCK_THRESHOLD,
        );
        let mut ticker = interval(CLOCK_TICK);
        ticker.tick().await;
        loop {
            ticker.tick().await;
            if watch.tick(SystemTime::now(), Instant::now()) && clock_tx.send(Cause::Woke).is_err()
            {
                return;
            }
        }
    });
    active.push("clock");

    #[cfg(all(target_os = "macos", feature = "netwatch"))]
    if platform::spawn(tx.clone()).is_ok() {
        active.push("system-configuration");
    }
    #[cfg(all(target_os = "linux", feature = "netwatch"))]
    if platform::spawn(tx.clone()).is_ok() {
        active.push("netlink");
    }
    drop(tx);
    Sources { events, active }
}

/// Waits for a cause, then for a quiet period, and returns the strongest cause seen.
///
/// A change of network produces many notifications within a short time, and one reaction is
/// enough. Returns `None` when all sources are gone.
pub async fn next_settled(
    events: &mut mpsc::UnboundedReceiver<Cause>,
    quiet: Duration,
) -> Option<Cause> {
    let mut cause = events.recv().await?;
    loop {
        let deadline = tokio::time::Instant::now() + quiet;
        match timeout_at(deadline, events.recv()).await {
            Ok(Some(more)) => cause = cause.max(more),
            Ok(None) | Err(_) => return Some(cause),
        }
    }
}

#[cfg(all(target_os = "macos", feature = "netwatch"))]
mod platform {
    use core_foundation::array::CFArray;
    use core_foundation::base::TCFType;
    use core_foundation::runloop::CFRunLoop;
    use core_foundation::string::CFString;
    use system_configuration::dynamic_store::{
        SCDynamicStore, SCDynamicStoreBuilder, SCDynamicStoreCallBackContext,
    };

    use super::Cause;
    use tokio::sync::mpsc::UnboundedSender;

    fn changed(_store: SCDynamicStore, _keys: CFArray<CFString>, tx: &mut UnboundedSender<Cause>) {
        let _ = tx.send(Cause::NetworkChanged);
    }

    /// Watches the network state on a thread of its own, which runs a CoreFoundation run loop.
    pub fn spawn(tx: UnboundedSender<Cause>) -> Result<(), String> {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("ssj-netwatch".to_owned())
            .spawn(move || {
                let store = SCDynamicStoreBuilder::new("ssj")
                    .callback_context(SCDynamicStoreCallBackContext {
                        callout: changed,
                        info: tx,
                    })
                    .build();
                let Some(store) = store else {
                    let _ = ready_tx.send(Err("cannot open the dynamic store".to_owned()));
                    return;
                };
                let keys = CFArray::<CFString>::from_CFTypes(&[
                    CFString::new("State:/Network/Global/IPv4"),
                    CFString::new("State:/Network/Global/IPv6"),
                ]);
                let patterns = CFArray::<CFString>::from_CFTypes(&[
                    CFString::new("State:/Network/Interface/.*/IPv4"),
                    CFString::new("State:/Network/Interface/.*/IPv6"),
                    CFString::new("State:/Network/Interface/.*/Link"),
                ]);
                if !store.set_notification_keys(&keys, &patterns) {
                    let _ = ready_tx.send(Err("cannot watch the network state".to_owned()));
                    return;
                }
                let Some(source) = store.create_run_loop_source() else {
                    let _ = ready_tx.send(Err("cannot create a run loop source".to_owned()));
                    return;
                };
                // the run loop finds a mode by the text of its name, so the name of the default
                // mode works without touching the extern static that holds it.
                let mode = CFString::new("kCFRunLoopDefaultMode");
                CFRunLoop::get_current().add_source(&source, mode.as_concrete_TypeRef());
                let _ = ready_tx.send(Ok(()));
                CFRunLoop::run_current();
            })
            .map_err(|e| e.to_string())?;
        ready_rx.recv().map_err(|e| e.to_string())?
    }
}

#[cfg(all(target_os = "linux", feature = "netwatch"))]
mod platform {
    use netlink_sys::{Socket, SocketAddr, protocols::NETLINK_ROUTE};
    use tokio::sync::mpsc::UnboundedSender;

    use super::Cause;

    const RTMGRP_LINK: u32 = 0x1;
    const RTMGRP_IPV4_IFADDR: u32 = 0x10;
    const RTMGRP_IPV4_ROUTE: u32 = 0x40;
    const RTMGRP_IPV6_IFADDR: u32 = 0x100;
    const RTMGRP_IPV6_ROUTE: u32 = 0x400;

    /// Listens for link, address, and route changes. The content of the messages does not
    /// matter: any message means that the network may look different now.
    pub fn spawn(tx: UnboundedSender<Cause>) -> Result<(), String> {
        let mut socket = Socket::new(NETLINK_ROUTE).map_err(|e| e.to_string())?;
        let groups = RTMGRP_LINK
            | RTMGRP_IPV4_IFADDR
            | RTMGRP_IPV4_ROUTE
            | RTMGRP_IPV6_IFADDR
            | RTMGRP_IPV6_ROUTE;
        socket
            .bind(&SocketAddr::new(0, groups))
            .map_err(|e| e.to_string())?;
        std::thread::Builder::new()
            .name("ssj-netwatch".to_owned())
            .spawn(move || {
                let mut buffer = vec![0u8; 8192];
                loop {
                    match socket.recv(&mut &mut buffer[..], 0) {
                        Ok(0) => return,
                        // an overflow of the socket also means that something changed.
                        Ok(_) | Err(_) if tx.send(Cause::NetworkChanged).is_err() => return,
                        Ok(_) => {}
                        Err(err) if err.raw_os_error() == Some(libc_enobufs()) => {}
                        Err(_) => return,
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// `ENOBUFS` on Linux.
    const fn libc_enobufs() -> i32 {
        105
    }
}
