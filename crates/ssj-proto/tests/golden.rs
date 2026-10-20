mod common;

use std::path::PathBuf;

use common::{client_frames, server_frames};
use ssj_proto::PROTOCOL;

fn encoded_frames() -> Vec<u8> {
    let mut out = Vec::new();
    for frame in client_frames() {
        frame.encode(&mut out).unwrap();
    }
    for frame in server_frames() {
        frame.encode(&mut out).unwrap();
    }
    out
}

// fails when the wire format changes without a new protocol version. To create the file for a
// new version, run with `UPDATE_GOLDEN=1` after increasing `PROTOCOL`. An existing file is never
// overwritten.
#[test]
fn wire_format_matches_the_golden_file_of_this_protocol_version() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(format!("protocol-{PROTOCOL}.bin"));
    let encoded = encoded_frames();

    match std::fs::read(&path) {
        Ok(golden) => assert!(
            golden == encoded,
            "the wire format changed: increase PROTOCOL in ssj-proto and store a new golden file"
        ),
        Err(_) if std::env::var_os("UPDATE_GOLDEN").is_some() => {
            std::fs::write(&path, &encoded).unwrap()
        }
        Err(_) => panic!("{} is missing: run with UPDATE_GOLDEN=1", path.display()),
    }
}
