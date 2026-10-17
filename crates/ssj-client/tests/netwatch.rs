use std::time::{Duration, Instant, SystemTime};

use ssj_client::netwatch::{Cause, ClockWatch, next_settled};
use tokio::sync::mpsc;
use tokio::time::timeout;

fn watch() -> (ClockWatch, SystemTime, Instant) {
    let (wall, mono) = (
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000),
        Instant::now(),
    );
    (
        ClockWatch::new(wall, mono, Duration::from_secs(1), Duration::from_secs(3)),
        wall,
        mono,
    )
}

#[test]
fn normal_ticks_and_small_delays_are_not_a_wake() {
    let (mut clock, wall, mono) = watch();
    let step = |n: u64| {
        (
            wall + Duration::from_millis(n),
            mono + Duration::from_millis(n),
        )
    };
    for (i, ms) in [1000, 2000, 3100, 5000, 8500].into_iter().enumerate() {
        let (w, m) = step(ms);
        // 8.5 s is 3.5 s after the one before, which is still within 1 s + 3 s
        assert!(!clock.tick(w, m), "tick {i}");
    }
}

#[test]
fn a_late_timer_is_a_wake_whichever_clock_saw_the_sleep() {
    // wall clock runs during sleep, the monotonic clock does not (Linux, older macOS)
    let (mut clock, wall, mono) = watch();
    assert!(clock.tick(
        wall + Duration::from_secs(600),
        mono + Duration::from_secs(1)
    ));
    // the monotonic clock runs during sleep, the wall clock was set back
    let (mut clock, wall, mono) = watch();
    assert!(clock.tick(
        wall - Duration::from_secs(5),
        mono + Duration::from_secs(600)
    ));
    // the very next tick is normal again
    assert!(!clock.tick(
        wall - Duration::from_secs(4),
        mono + Duration::from_secs(601)
    ));
}

// A wall clock that jumps backwards must not be mistaken for a wake, or crash.
#[test]
fn clock_set_back_is_ignored() {
    let (mut clock, wall, mono) = watch();
    assert!(!clock.tick(
        wall - Duration::from_secs(3600),
        mono + Duration::from_secs(1)
    ));
}

#[tokio::test(start_paused = true)]
async fn one_reaction_for_a_burst_of_notifications_with_the_strongest_cause() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    for cause in [
        Cause::NetworkChanged,
        Cause::NetworkChanged,
        Cause::Woke,
        Cause::NetworkChanged,
    ] {
        tx.send(cause).unwrap();
    }
    let quiet = Duration::from_millis(300);
    assert_eq!(next_settled(&mut rx, quiet).await, Some(Cause::Woke));
    // nothing is left over
    assert!(
        timeout(Duration::from_secs(5), next_settled(&mut rx, quiet))
            .await
            .is_err()
    );
}

#[tokio::test(start_paused = true)]
async fn a_slow_burst_extends_the_wait_and_the_end_of_all_sources_ends_the_wait() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    tx.send(Cause::NetworkChanged).unwrap();
    let sender = tokio::spawn(async move {
        for _ in 0..3 {
            tokio::time::sleep(Duration::from_millis(200)).await;
            tx.send(Cause::NetworkChanged).unwrap();
        }
    });
    let started = tokio::time::Instant::now();
    assert_eq!(
        next_settled(&mut rx, Duration::from_millis(300)).await,
        Some(Cause::NetworkChanged)
    );
    // the pieces arrived at 200, 400, and 600 ms, and the last sender ended there, which ends the
    // wait at once: without any source left there is nothing more to wait for
    assert_eq!(started.elapsed(), Duration::from_millis(600));
    sender.await.unwrap();
    assert_eq!(
        next_settled(&mut rx, Duration::from_millis(300)).await,
        None
    );
}

#[tokio::test]
async fn the_clock_source_is_always_started() {
    let sources = ssj_client::netwatch::start();
    assert!(sources.active.contains(&"clock"), "{:?}", sources.active);
}

#[cfg(all(target_os = "macos", feature = "netwatch"))]
#[tokio::test]
async fn the_macos_source_starts_and_stays_quiet_without_a_change() {
    let mut sources = ssj_client::netwatch::start();
    assert!(
        sources.active.contains(&"system-configuration"),
        "{:?}",
        sources.active
    );
    // no notification for the start itself
    assert!(
        timeout(Duration::from_millis(800), sources.events.recv())
            .await
            .is_err()
    );
}

// The state of the network cannot be changed without privileges, so this one is run by hand:
// `cargo test -p ssj-client --test netwatch -- --ignored --nocapture`, then switch Wi-Fi off.
#[cfg(all(target_os = "macos", feature = "netwatch"))]
#[tokio::test]
#[ignore = "needs a real change of the network"]
async fn a_real_change_of_the_network_is_noticed_on_macos() {
    let mut sources = ssj_client::netwatch::start();
    println!("switch Wi-Fi off or on within 60 seconds");
    let cause = timeout(Duration::from_secs(60), sources.events.recv())
        .await
        .expect("no change was noticed");
    assert_eq!(cause, Some(Cause::NetworkChanged));
}
