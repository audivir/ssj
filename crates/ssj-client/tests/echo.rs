use std::time::{Duration, Instant};

use ssj_client::echo::{EXPIRE, Echo};

const FAINT_B: &[u8] = b"\x1b[2mb\x1b[22m";
const RETRACT_1: &[u8] = b"\x08\x1b[K";

/// An `Echo` at a prompt that the server has drawn, with the first typed character echoed, so
/// that it trusts the line.
fn trusted(now: Instant) -> Echo {
    let mut echo = Echo::new(true, 80);
    echo.output(b"\r\nme@host:~$ ", now);
    assert!(
        echo.typed(b"a", now).is_empty(),
        "the first character is not shown"
    );
    let (before, after) = echo.output(b"a", now);
    assert!(before.is_empty() && after.is_empty());
    echo
}

#[test]
fn a_character_is_shown_after_the_server_echoed_one_and_replaced_by_its_echo() {
    let now = Instant::now();
    let mut echo = trusted(now);
    assert_eq!(echo.typed(b"b", now), FAINT_B);
    // the echo arrives: the drawing is taken back and the real character goes over it
    let (before, after) = echo.output(b"b", now);
    assert_eq!(before, RETRACT_1);
    assert!(after.is_empty());
    // and the next one is shown again
    assert_eq!(echo.typed(b"b", now), FAINT_B);
}

#[test]
fn what_is_still_waiting_is_drawn_again_after_a_partial_echo() {
    let now = Instant::now();
    let mut echo = trusted(now);
    let mut drawn = echo.typed(b"b", now);
    drawn.extend(echo.typed(b"c", now));
    assert_eq!(drawn, b"\x1b[2mb\x1b[22m\x1b[2mc\x1b[22m");
    let (before, after) = echo.output(b"b", now);
    assert_eq!(before, b"\x08\x08\x1b[K");
    assert_eq!(after, b"\x1b[2mc\x1b[22m");
    let (before, after) = echo.output(b"c", now);
    assert_eq!(before, RETRACT_1);
    assert!(after.is_empty());
}

#[test]
fn colors_and_titles_in_the_echo_keep_the_trust() {
    let now = Instant::now();
    let mut echo = trusted(now);
    echo.typed(b"b", now);
    echo.output(b"\x1b[32mb\x1b[0m\x1b]2;title\x07\x1b[?2004h", now);
    assert_eq!(echo.typed(b"x", now), b"\x1b[2mx\x1b[22m");
}

#[test]
fn nothing_is_shown_for_a_program_that_does_not_echo() {
    let now = Instant::now();
    let mut echo = Echo::new(true, 80);
    echo.output(b"\r\nPassword: ", now);
    for byte in b"hunter2" {
        assert!(echo.typed(&[*byte], now).is_empty());
    }
    assert!(echo.due().is_none());
}

#[test]
fn enter_ends_the_trust_until_the_next_echo() {
    let now = Instant::now();
    let mut echo = trusted(now);
    assert!(echo.typed(b"\r", now).is_empty());
    echo.output(b"\r\nsecret prompt: ", now);
    assert!(echo.typed(b"p", now).is_empty());
    assert!(echo.typed(b"q", now).is_empty());
}

#[test]
fn a_different_answer_takes_everything_back() {
    let now = Instant::now();
    let mut echo = trusted(now);
    echo.typed(b"bc", now);
    let (before, after) = echo.output(b"X", now);
    assert_eq!(before, b"\x08\x08\x1b[K");
    assert!(after.is_empty());
    assert!(echo.typed(b"d", now).is_empty(), "the trust is gone");
}

#[test]
fn a_cursor_movement_or_a_full_screen_program_stops_the_prediction() {
    for sequence in [
        &b"\x1b[2D"[..],
        b"\x1b[?1049h",
        b"\x1b[H",
        b"\x1b7",
        b"\x08",
        b"\t",
    ] {
        let now = Instant::now();
        let mut echo = trusted(now);
        echo.output(sequence, now);
        assert!(echo.typed(b"b", now).is_empty(), "{sequence:?}");
        echo.output(b"b", now);
        assert!(
            echo.typed(b"b", now).is_empty(),
            "{sequence:?}: not before Enter"
        );
    }
}

#[test]
fn a_sequence_split_between_two_chunks_is_followed() {
    let now = Instant::now();
    let mut echo = trusted(now);
    echo.typed(b"b", now);
    echo.output(b"b\x1b[3", now);
    echo.output(b"2m", now);
    assert_eq!(echo.typed(b"x", now), b"\x1b[2mx\x1b[22m");
    let (_, _) = echo.output(b"\x1b[2", now);
    echo.output(b"D", now);
    assert!(echo.typed(b"x", now).is_empty());
}

#[test]
fn other_keys_stop_the_prediction_until_enter() {
    for key in [&b"\x7f"[..], b"\x1b[A", b"\x03", "é".as_bytes()] {
        let now = Instant::now();
        let mut echo = trusted(now);
        echo.typed(key, now);
        echo.output(b"b", now);
        assert!(echo.typed(b"b", now).is_empty(), "{key:?}");
        echo.typed(b"\r", now);
        echo.output(b"\r\n$ ", now);
        echo.typed(b"a", now);
        echo.output(b"a", now);
        assert!(
            !echo.typed(b"b", now).is_empty(),
            "{key:?}: Enter restores it"
        );
    }
}

#[test]
fn nothing_is_drawn_near_the_right_edge() {
    let now = Instant::now();
    let mut echo = Echo::new(true, 20);
    echo.output(b"\r\n$ ", now);
    echo.typed(b"a", now);
    echo.output(b"a", now);
    let mut shown = 0;
    for _ in 0..30 {
        if !echo.typed(b"b", now).is_empty() {
            shown += 1;
        }
    }
    // the cursor is in column 3, and the drawing stops with a few columns left at the edge
    assert_eq!(shown, 13);
}

#[test]
fn a_paste_is_not_followed() {
    let now = Instant::now();
    let mut echo = trusted(now);
    let drawn = echo.typed(&[b'x'; 5000], now);
    assert!(drawn.is_empty());
    assert!(echo.typed(b"y", now).is_empty());
}

#[test]
fn an_unanswered_prediction_runs_out() {
    let now = Instant::now();
    let mut echo = trusted(now);
    echo.typed(b"bc", now);
    assert_eq!(echo.due(), Some(now + EXPIRE));
    assert!(
        echo.expire(now + EXPIRE - Duration::from_millis(1))
            .is_empty()
    );
    assert_eq!(echo.expire(now + EXPIRE), b"\x08\x08\x1b[K");
    assert!(echo.due().is_none());
    assert!(echo.typed(b"d", now).is_empty(), "the trust is gone");
}

#[test]
fn off_by_default_means_no_bytes_at_all() {
    let now = Instant::now();
    let mut echo = Echo::new(false, 80);
    echo.output(b"\r\n$ ", now);
    for _ in 0..3 {
        assert!(echo.typed(b"a", now).is_empty());
        let (before, after) = echo.output(b"a", now);
        assert!(before.is_empty() && after.is_empty());
    }
    assert!(echo.cancel().is_empty());
}
