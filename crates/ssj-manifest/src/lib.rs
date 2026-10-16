//! The format of a slot inside the client binary that holds the SHA-256 digests of the server files.
//!
//! The client is built with an empty slot. The release pipeline builds the servers, then writes
//! their digests into the slot of each client binary with `ssj-manifest` (crate
//! `ssj-manifest-tool`). The client reads its own slot and refuses a server file that does not
//! match.
//!
//! Layout: the marker, a big endian entry count, and `MAX_ENTRIES` entries of a name (ASCII,
//! padded with zero bytes) and a digest. Everything after the marker is zero in the empty slot.
//!
//! The tool finds the slot by searching for the whole marker, which must occur exactly once in
//! the client. So the client must not hold the marker anywhere but at the start of its slot: the
//! marker is split into two halves here, and no code of this crate builds or compares the whole.

use sha2::{Digest, Sha256};

///
/// The first half is stored masked and unmasked at run time. The compiler tends to lay two
/// neighbouring constants out next to each other, which would put the whole marker into the
/// program a second time, and a masked half cannot join its neighbour into the marker.
pub const TAIL: [u8; 12] = *b"-SLOT-V1\0\0\0\0";
const MASK: u8 = 0x5a;
const HEAD_MASKED: [u8; 12] = mask(*b"SSJ-MANIFEST");

const fn mask(bytes: [u8; 12]) -> [u8; 12] {
    let mut out = [0u8; 12];
    let mut i = 0;
    while i < 12 {
        out[i] = bytes[i] ^ MASK;
        i += 1;
    }
    out
}

/// The first half of the marker.
pub fn head() -> [u8; 12] {
    mask(HEAD_MASKED)
}
pub const MARKER_LEN: usize = 24;
pub const MAX_ENTRIES: usize = 8;
pub const NAME_LEN: usize = 40;
const ENTRY_LEN: usize = NAME_LEN + 32;
/// Bytes of the slot after the marker.
pub const BODY_LEN: usize = 2 + MAX_ENTRIES * ENTRY_LEN;
pub const SLOT_LEN: usize = MARKER_LEN + BODY_LEN;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ManifestError {
    /// The bytes are not a slot of this format.
    NotASlot,
    /// The slot has more entries than it can hold, or the count and the content disagree.
    BadCount,
    /// A name is empty, too long, or contains characters outside `a-z`, `0-9`, `-`, and `_`.
    BadName(String),
    DuplicateName(String),
    TooManyEntries,
    /// The marker is not in the file, or more than once.
    MarkerCount(usize),
}

impl std::fmt::Display for ManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotASlot => write!(f, "not a manifest slot"),
            Self::BadCount => write!(f, "the slot has an invalid entry count"),
            Self::BadName(name) => write!(f, "invalid build name {name:?}"),
            Self::DuplicateName(name) => write!(f, "build {name:?} appears twice"),
            Self::TooManyEntries => write!(f, "more than {MAX_ENTRIES} entries"),
            Self::MarkerCount(n) => write!(f, "the marker occurs {n} times, expected exactly once"),
        }
    }
}

impl std::error::Error for ManifestError {}

/// The slot of a build that was not patched: the marker and zeros.
pub const fn empty_slot() -> [u8; SLOT_LEN] {
    let mut slot = [0u8; SLOT_LEN];
    let mut i = 0;
    let head = mask(HEAD_MASKED);
    while i < 12 {
        slot[i] = head[i];
        slot[12 + i] = TAIL[i];
        i += 1;
    }
    slot
}

/// Digest of the server file of one build, such as `x86_64-unknown-linux-musl`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub digest: [u8; 32],
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Manifest(Vec<Entry>);

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= NAME_LEN
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

impl Manifest {
    pub fn new(entries: Vec<Entry>) -> Result<Self, ManifestError> {
        if entries.len() > MAX_ENTRIES {
            return Err(ManifestError::TooManyEntries);
        }
        for (i, entry) in entries.iter().enumerate() {
            if !valid_name(&entry.name) {
                return Err(ManifestError::BadName(entry.name.clone()));
            }
            if entries[..i].iter().any(|other| other.name == entry.name) {
                return Err(ManifestError::DuplicateName(entry.name.clone()));
            }
        }
        Ok(Self(entries))
    }

    pub fn entries(&self) -> &[Entry] {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn lookup(&self, name: &str) -> Option<[u8; 32]> {
        self.0.iter().find(|e| e.name == name).map(|e| e.digest)
    }

    /// Reads a slot, strictly: unused bytes must be zero, and names must be well formed.
    pub fn parse(slot: &[u8]) -> Result<Self, ManifestError> {
        if slot.len() != SLOT_LEN || slot[..12] != head() || slot[12..MARKER_LEN] != TAIL {
            return Err(ManifestError::NotASlot);
        }
        let body = &slot[MARKER_LEN..];
        let count = usize::from(u16::from_be_bytes([body[0], body[1]]));
        if count > MAX_ENTRIES {
            return Err(ManifestError::BadCount);
        }
        let mut entries = Vec::new();
        for (index, raw) in body[2..].chunks(ENTRY_LEN).enumerate() {
            if index >= count {
                if raw.iter().any(|b| *b != 0) {
                    return Err(ManifestError::BadCount);
                }
                continue;
            }
            let (name_bytes, digest) = raw.split_at(NAME_LEN);
            let used = name_bytes.iter().position(|b| *b == 0).unwrap_or(NAME_LEN);
            let bad = || ManifestError::BadName(String::from_utf8_lossy(name_bytes).into_owned());
            if name_bytes[used..].iter().any(|b| *b != 0) {
                return Err(bad());
            }
            let name = std::str::from_utf8(&name_bytes[..used])
                .map_err(|_| bad())?
                .to_owned();
            entries.push(Entry {
                name,
                digest: digest.try_into().expect("32 bytes"),
            });
        }
        Self::new(entries)
    }

    pub fn encode(&self) -> [u8; SLOT_LEN] {
        let mut slot = empty_slot();
        let body = &mut slot[MARKER_LEN..];
        body[..2].copy_from_slice(&(self.0.len() as u16).to_be_bytes());
        for (index, entry) in self.0.iter().enumerate() {
            let start = 2 + index * ENTRY_LEN;
            body[start..start + entry.name.len()].copy_from_slice(entry.name.as_bytes());
            body[start + NAME_LEN..start + ENTRY_LEN].copy_from_slice(&entry.digest);
        }
        slot
    }
}

pub fn digest(file: &[u8]) -> [u8; 32] {
    Sha256::digest(file).into()
}

pub fn hex(digest: &[u8; 32]) -> String {
    digest.iter().map(|b| format!("{b:02x}")).collect()
}
