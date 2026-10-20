// fuzz target. The first line is a comment, so that the file does not look like a script.

#![no_main]

use libfuzzer_sys::fuzz_target;
use ssj_proto::destination::parse;

// what is accepted is safe for `ssh` and stable: no option, no space or control character, and
// the canonical form parses back to the same destination.
fuzz_target!(|text: &str| {
    if let Ok(destination) = parse(text) {
        let target = destination.target();
        assert!(!target.is_empty() && !target.starts_with('-'));
        assert!(target.bytes().all(|b| b.is_ascii_graphic()));
        assert_eq!(parse(&destination.to_string()), Ok(destination));
    }
});
