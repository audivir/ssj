use ssj_manifest::{
    BODY_LEN, Entry, MARKER_LEN, Manifest, ManifestError, NAME_LEN, SLOT_LEN, TAIL, empty_slot,
    head,
};

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

#[test]
fn empty_slot_is_the_marker_and_zeros_and_reads_as_empty() {
    let slot = empty_slot();
    assert_eq!(slot[..12], head());
    assert_eq!(slot[12..MARKER_LEN], TAIL);
    assert!(slot[MARKER_LEN..].iter().all(|b| *b == 0));
    assert_eq!(slot.len(), MARKER_LEN + BODY_LEN);
    assert!(Manifest::parse(&slot).unwrap().is_empty());
}

#[test]
fn entries_survive_a_round_trip_in_order() {
    let slot = manifest().encode();
    let parsed = Manifest::parse(&slot).unwrap();
    assert_eq!(parsed, manifest());
    assert_eq!(parsed.lookup("aarch64-apple-darwin"), Some([2; 32]));
    assert_eq!(parsed.lookup("riscv64"), None);
    // a name as long as the field has no terminator
    let longest = "a".repeat(NAME_LEN);
    let full = Manifest::new(vec![entry(&longest, 9)]).unwrap();
    assert_eq!(Manifest::parse(&full.encode()).unwrap(), full);
}

#[test]
fn builders_refuse_what_a_slot_cannot_hold() {
    assert!(matches!(
        Manifest::new(vec![entry("Upper", 0)]),
        Err(ManifestError::BadName(_))
    ));
    assert!(matches!(
        Manifest::new(vec![entry("", 0)]),
        Err(ManifestError::BadName(_))
    ));
    assert!(matches!(
        Manifest::new(vec![entry("a b", 0)]),
        Err(ManifestError::BadName(_))
    ));
    assert!(matches!(
        Manifest::new(vec![entry(&"a".repeat(NAME_LEN + 1), 0)]),
        Err(ManifestError::BadName(_))
    ));
    assert!(matches!(
        Manifest::new(vec![entry("a", 0), entry("a", 1)]),
        Err(ManifestError::DuplicateName(_))
    ));
    let nine: Vec<Entry> = (0..9).map(|i| entry(&format!("b{i}"), 0)).collect();
    assert_eq!(Manifest::new(nine), Err(ManifestError::TooManyEntries));
}

// The client trusts its slot, so a slot that is not exactly what the tool writes is refused.
type Mutation = Box<dyn Fn(&mut [u8])>;

#[test]
fn parsing_is_strict() {
    let good = manifest().encode();
    let at = |offset: usize| MARKER_LEN + offset;
    let mutations: Vec<(&str, Mutation)> = vec![
        ("wrong marker", Box::new(|s| s[0] ^= 1)),
        ("count above the maximum", Box::new(move |s| s[at(1)] = 9)),
        (
            "junk behind the last entry",
            Box::new(move |s| s[at(2 + 5 * 72)] = 1),
        ),
        (
            "junk after the end of a name",
            Box::new(move |s| s[at(2) + 30] = b'x'),
        ),
        ("a name with a space", Box::new(move |s| s[at(2)] = b' ')),
        (
            "a name that is not utf-8",
            Box::new(move |s| s[at(2)] = 0xff),
        ),
        ("an empty first name", Box::new(move |s| s[at(2)] = 0)),
    ];
    for (name, mutate) in mutations {
        let mut slot = good;
        mutate(&mut slot);
        assert!(Manifest::parse(&slot).is_err(), "{name}");
    }
    assert!(Manifest::parse(&good[..SLOT_LEN - 1]).is_err());
    assert!(Manifest::parse(&[good.as_slice(), &[0]].concat()).is_err());
}
