#![no_main]

use libfuzzer_sys::fuzz_target;
use ssj_proto::control::{format_reply, format_request, parse_reply, parse_request};

// Whatever parses must format back to the same line.
fuzz_target!(|data: &[u8]| {
    if let Ok(request) = parse_request(data) {
        let line = format_request(&request);
        assert_eq!(line.as_bytes(), data.strip_suffix(b"\n").map_or(data, |d| d).iter().chain(b"\n").copied().collect::<Vec<u8>>());
    }
    if let Ok(reply) = parse_reply(data) {
        let line = format_reply(&reply);
        let mut expected = data.to_vec();
        if !expected.ends_with(b"\n") {
            expected.push(b'\n');
        }
        assert_eq!(line.as_bytes(), expected);
    }
});
