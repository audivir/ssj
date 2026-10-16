#![no_main]

use libfuzzer_sys::fuzz_target;
use ssj_proto::decode_client;

// Anything that decodes must encode back to exactly the bytes it came from.
fuzz_target!(|data: &[u8]| {
    if let Ok(Some((frame, used))) = decode_client(data) {
        assert!(used <= data.len());
        let mut again = Vec::new();
        frame.encode(&mut again).unwrap();
        assert_eq!(again, &data[..used]);
    }
});
