use std::time::Duration;

use ssj_client::debounce::Debounce;
use ssj_proto::WindowSize;
use tokio::time::Instant;

fn size(rows: u16) -> WindowSize {
    WindowSize::new(rows, 80).unwrap()
}

#[test]
fn a_burst_gives_one_size_after_it_stops_and_it_is_the_last() {
    let quiet = Duration::from_millis(50);
    let mut debounce = Debounce::new(quiet);
    let start = Instant::now();
    assert_eq!(debounce.due(), None);

    // a new size every 20 ms keeps pushing the due time out.
    for i in 0..10u64 {
        let now = start + Duration::from_millis(20 * i);
        debounce.push(size(30 + i as u16), now);
        assert_eq!(debounce.take(now + Duration::from_millis(49)), None);
    }
    let last = start + Duration::from_millis(180);
    assert_eq!(debounce.due(), Some(last + quiet));
    assert_eq!(debounce.take(last + quiet), Some(size(39)));
    // delivered once
    assert_eq!(debounce.take(last + quiet), None);
    assert_eq!(debounce.due(), None);
}
