// fuzz target. The first line is a comment, so that the file does not look like a script.

#![no_main]

use libfuzzer_sys::fuzz_target;
use ssj_proto::decode_server;

// anything that decodes must encode to bytes that decode to the same frame. The bytes may differ
// from the input: postcard also takes a number written longer than it needs to be.
fuzz_target!(|data: &[u8]| {
    if let Ok(Some((frame, used))) = decode_server(data) {
        assert!(used <= data.len());
        let mut again = Vec::new();
        frame.encode(&mut again).unwrap();
        assert_eq!(decode_server(&again), Ok(Some((frame, again.len()))));
    }
});
