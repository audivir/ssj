//! Finding and filling the manifest slot in a client binary.
//!
//! This code searches for the whole marker, so it must never be linked into the client. That is
//! why it is not part of the `ssj-manifest` crate that the client uses.

use sha2::{Digest, Sha256};
use ssj_manifest::{BODY_LEN, MARKER_LEN, Manifest, ManifestError, SLOT_LEN, TAIL, head};

/// The whole marker, built at run time.
pub fn marker() -> [u8; MARKER_LEN] {
    let mut out = [0u8; MARKER_LEN];
    out[..12].copy_from_slice(&head());
    out[12..].copy_from_slice(&TAIL);
    out
}

/// Offset of the slot in `file`. The marker has to occur exactly once.
pub fn find_slot(file: &[u8]) -> Result<usize, ManifestError> {
    let marker = marker();
    let mut found = file
        .windows(MARKER_LEN)
        .enumerate()
        .filter(|(_, w)| *w == marker);
    let first = found.next().map(|(offset, _)| offset);
    let more = found.count();
    match first {
        Some(offset) if more == 0 && offset + SLOT_LEN <= file.len() => Ok(offset),
        Some(_) => Err(ManifestError::MarkerCount(1 + more)),
        None => Err(ManifestError::MarkerCount(0)),
    }
}

/// Writes `manifest` into the slot of `file`.
pub fn patch(file: &mut [u8], manifest: &Manifest) -> Result<(), ManifestError> {
    let offset = find_slot(file)?;
    file[offset..offset + SLOT_LEN].copy_from_slice(&manifest.encode());
    Ok(())
}

pub fn read(file: &[u8]) -> Result<Manifest, ManifestError> {
    let offset = find_slot(file)?;
    Manifest::parse(&file[offset..offset + SLOT_LEN])
}

/// SHA-256 of `file` with the content of the slot (everything after the marker) set to zero.
///
/// The digest of a patched client equals the digest of the same build before patching, so the
/// content of two builds can be compared although their slots differ.
pub fn digest_ignoring_slot(file: &[u8]) -> Result<[u8; 32], ManifestError> {
    let offset = find_slot(file)?;
    let body = offset + MARKER_LEN;
    let mut hasher = Sha256::new();
    hasher.update(&file[..body]);
    hasher.update([0u8; BODY_LEN]);
    hasher.update(&file[body + BODY_LEN..]);
    Ok(hasher.finalize().into())
}
