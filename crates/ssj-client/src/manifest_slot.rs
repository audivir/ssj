//! The slot for the digests of the server files, filled by the release pipeline.

use std::hint::black_box;

use ssj_manifest::{Manifest, SLOT_LEN, empty_slot};

/// Empty in a normal build. `ssj-manifest patch` writes the digests into the finished binary.
static SLOT: [u8; SLOT_LEN] = empty_slot();

/// The digests this client was released with. Empty for a build that was not patched.
pub fn embedded() -> Result<Manifest, String> {
    // black_box keeps the compiler from assuming that the slot still holds its initial zeros
    Manifest::parse(black_box(&SLOT))
        .map_err(|e| format!("the manifest inside this client is damaged: {e}"))
}
