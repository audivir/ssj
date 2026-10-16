#![no_main]

use libfuzzer_sys::fuzz_target;
use ssj_proto::bootstrap::{format_connect, parse_connect, sanitize};

// A parsed line must be present in the input exactly as the client would format it.
fuzz_target!(|data: &[u8]| {
    if let Ok(Some(connect)) = parse_connect(data) {
        let line = format_connect(&connect);
        let line = line.trim_end().as_bytes();
        assert!(data.windows(line.len()).any(|w| w == line));
    }
    let shown = sanitize(data, 80);
    assert!(!shown.chars().any(|c| c.is_control()));
    assert!(shown.chars().count() <= 83);
});
