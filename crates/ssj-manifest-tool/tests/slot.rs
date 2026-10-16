use ssj_manifest::{Entry, MARKER_LEN, Manifest, ManifestError, SLOT_LEN, digest, empty_slot};
use ssj_manifest_tool::{digest_ignoring_slot, find_slot, marker, patch, read};

fn entry(name: &str, byte: u8) -> Entry {
    Entry {
        name: name.to_owned(),
        digest: [byte; 32],
    }
}

fn manifest() -> Manifest {
    Manifest::new(vec![
        entry("x86_64-unknown-linux-musl", 1),
        entry("aarch64-apple-darwin", 2),
    ])
    .unwrap()
}

/// A file with the slot somewhere in the middle, like a binary.
fn file_with_slot(slot: &[u8]) -> Vec<u8> {
    let mut file = b"code before ".repeat(50);
    file.extend_from_slice(slot);
    file.extend_from_slice(&b" code after".repeat(50));
    file
}

#[test]
fn the_marker_must_occur_exactly_once() {
    let one = file_with_slot(&empty_slot());
    assert!(find_slot(&one).is_ok());

    assert_eq!(
        find_slot(b"no marker here"),
        Err(ManifestError::MarkerCount(0))
    );

    let two = [one.clone(), empty_slot().to_vec()].concat();
    assert_eq!(find_slot(&two), Err(ManifestError::MarkerCount(2)));

    // a second copy of the marker alone, such as in a string of the program, is already too many
    let stray = [one, marker().to_vec()].concat();
    assert_eq!(find_slot(&stray), Err(ManifestError::MarkerCount(2)));

    // a slot cut off by the end of the file
    let cut = [b"x".to_vec(), empty_slot()[..SLOT_LEN - 10].to_vec()].concat();
    assert!(find_slot(&cut).is_err());
}

#[test]
fn patching_changes_nothing_but_the_slot() {
    let original = file_with_slot(&empty_slot());
    let mut patched = original.clone();
    patch(&mut patched, &manifest()).unwrap();

    assert_eq!(patched.len(), original.len());
    let offset = find_slot(&original).unwrap();
    assert_eq!(
        patched[..offset + MARKER_LEN],
        original[..offset + MARKER_LEN]
    );
    assert_eq!(patched[offset + SLOT_LEN..], original[offset + SLOT_LEN..]);
    assert_eq!(read(&patched).unwrap(), manifest());

    // patching again replaces the entries and does not append
    patch(
        &mut patched,
        &Manifest::new(vec![entry("only", 7)]).unwrap(),
    )
    .unwrap();
    assert_eq!(read(&patched).unwrap().entries().len(), 1);
}

#[test]
fn digest_ignoring_the_slot_is_the_same_before_and_after_patching() {
    let original = file_with_slot(&empty_slot());
    let mut patched = original.clone();
    patch(&mut patched, &manifest()).unwrap();

    assert_ne!(digest(&original), digest(&patched));
    assert_eq!(
        digest_ignoring_slot(&original).unwrap(),
        digest_ignoring_slot(&patched).unwrap()
    );

    // but any other change shows
    let mut changed = patched.clone();
    changed[3] ^= 1;
    assert_ne!(
        digest_ignoring_slot(&changed).unwrap(),
        digest_ignoring_slot(&patched).unwrap()
    );
    let mut after = patched.clone();
    *after.last_mut().unwrap() ^= 1;
    assert_ne!(
        digest_ignoring_slot(&after).unwrap(),
        digest_ignoring_slot(&patched).unwrap()
    );
}
