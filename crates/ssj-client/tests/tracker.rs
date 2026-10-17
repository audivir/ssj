use ssj_client::tracker::Tracker;

/// Whether the tracker is inside a sequence after each prefix of `bytes`.
fn states(bytes: &[u8]) -> Vec<bool> {
    let mut tracker = Tracker::new();
    bytes
        .iter()
        .map(|b| {
            tracker.feed(&[*b]);
            tracker.in_sequence()
        })
        .collect()
}

#[test]
fn plain_text_is_never_inside_a_sequence() {
    assert!(states(b"hello world\r\n").iter().all(|s| !s));
}

#[test]
fn a_sequence_is_open_from_its_first_to_its_last_byte() {
    // each byte of the sequence leaves the tracker inside, except the final one
    for sequence in [
        &b"\x1b[38;2;255;128;64m"[..],
        &b"\x1b[?1049h"[..],
        &b"\x1b]0;a title\x07"[..],
        &b"\x1b]52;c;ZXhhbXBsZQ==\x1b\\"[..],
        &b"\x1b_Ga=T,f=100;AAAA\x1b\\"[..],
        &b"\x1bPq#0;2;0;0;0\x1b\\"[..],
        &b"\x1b(B"[..],
    ] {
        let seen = states(sequence);
        let (last, inner) = seen.split_last().unwrap();
        assert!(inner.iter().all(|s| *s), "{sequence:?}");
        assert!(!last, "{sequence:?} must be complete at its end");
    }
}

#[test]
fn multi_byte_characters_are_not_split() {
    // two, three, and four bytes
    for text in ["é", "€", "😀"] {
        let seen = states(text.as_bytes());
        let (last, inner) = seen.split_last().unwrap();
        assert!(inner.iter().all(|s| *s), "{text}");
        assert!(!last, "{text}");
    }
    // a broken character does not leave the tracker stuck
    let mut tracker = Tracker::new();
    tracker.feed(b"\xe2\x82a");
    assert!(!tracker.in_sequence());
}

// A cancelled or abandoned sequence must not leave the status disabled forever.
#[test]
fn cancel_and_new_escape_end_a_sequence() {
    for bytes in [
        &b"\x1b[12\x18"[..],
        &b"\x1b]0;x\x1a"[..],
        &b"\x1b[12\x1b[0m"[..],
        &b"\x1b]0;x\x1b[0m"[..],
    ] {
        let mut tracker = Tracker::new();
        tracker.feed(bytes);
        assert!(!tracker.in_sequence(), "{bytes:?}");
    }
}

#[test]
fn chunk_boundaries_do_not_matter() {
    let data = "ab\x1b[1;31mred\x1b[0m \x1b]2;t\x07 é €\x1b_x\x1b\\".as_bytes();
    let whole = states(data);
    for size in [1, 2, 3, 7] {
        let mut tracker = Tracker::new();
        let mut at_chunk_ends = Vec::new();
        let mut end = 0;
        for chunk in data.chunks(size) {
            tracker.feed(chunk);
            end += chunk.len();
            at_chunk_ends.push((end, tracker.in_sequence()));
        }
        for (end, inside) in at_chunk_ends {
            assert_eq!(inside, whole[end - 1], "chunks of {size}, at {end}");
        }
    }
}

#[test]
fn the_last_title_is_remembered_and_only_when_it_is_complete() {
    let mut tracker = Tracker::new();
    assert_eq!(tracker.title(), None);
    tracker.feed(b"\x1b]0;first\x07");
    assert_eq!(tracker.title(), Some("first"));
    tracker.feed(b"\x1b]2;second\x1b\\");
    assert_eq!(tracker.title(), Some("second"));
    // other sequences and unfinished titles change nothing
    tracker.feed(b"\x1b]7;file:///x\x07\x1b]1;icon\x07\x1b]0;unfinished");
    assert_eq!(tracker.title(), Some("second"));
    tracker.feed(b"\x18");
    assert_eq!(tracker.title(), Some("second"));
    // a title with control characters is cleaned
    tracker.feed(b"\x1b]0;a\x01b\x07");
    assert_eq!(tracker.title(), Some("ab"));
    // a huge title is ignored, and the tracker still finds the end of it
    let mut huge = b"\x1b]0;".to_vec();
    huge.extend(std::iter::repeat_n(b'x', 5000));
    huge.push(0x07);
    tracker.feed(&huge);
    assert_eq!(tracker.title(), Some("ab"));
    assert!(!tracker.in_sequence());
}
