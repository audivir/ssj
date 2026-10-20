use ssj_client::select::{Key, Menu, Outcome, parse_keys};

#[test]
fn keys_are_read_from_the_bytes_of_one_read() {
    assert_eq!(
        parse_keys(b"\x1b[B\x1b[A\x1bOB"),
        [Key::Down, Key::Up, Key::Down]
    );
    assert_eq!(
        parse_keys(b"jkjp"),
        [Key::Down, Key::Up, Key::Down, Key::Up]
    );
    assert_eq!(parse_keys(b"\r"), [Key::Enter]);
    assert_eq!(parse_keys(b"3"), [Key::Number(3)]);
    for cancel in [&b"q"[..], b"\x03", b"\x04", b"\x1b"] {
        assert_eq!(parse_keys(cancel), [Key::Cancel], "{cancel:?}");
    }
    // other sequences, such as the arrows to the sides or Delete, do nothing.
    assert_eq!(parse_keys(b"\x1b[C\x1b[3~\x1b[D"), []);
    assert_eq!(parse_keys(b"\x1b[C\r"), [Key::Enter]);
    assert_eq!(parse_keys(b"x \t"), []);
}

#[test]
fn the_selection_moves_within_the_lines_and_enter_chooses() {
    let mut menu = Menu::new(vec!["a".into(), "b".into(), "new".into()]);
    assert_eq!(menu.selected(), 0);
    assert_eq!(menu.press(Key::Up), Outcome::Open);
    assert_eq!(menu.selected(), 0, "no wrapping above the first line");
    assert_eq!(menu.press(Key::Down), Outcome::Open);
    assert_eq!(menu.press(Key::Down), Outcome::Open);
    assert_eq!(menu.press(Key::Down), Outcome::Open);
    assert_eq!(menu.selected(), 2, "no wrapping below the last line");
    assert_eq!(menu.press(Key::Number(2)), Outcome::Open);
    assert_eq!(menu.selected(), 1);
    assert_eq!(menu.press(Key::Number(9)), Outcome::Open);
    assert_eq!(menu.selected(), 1, "a number past the end does nothing");
    assert_eq!(menu.press(Key::Enter), Outcome::Chosen(1));
    assert_eq!(menu.press(Key::Cancel), Outcome::Cancelled);
}

#[test]
fn the_selected_line_is_marked_and_every_line_ends_the_row() {
    let mut menu = Menu::new(vec!["first".into(), "second".into()]);
    menu.press(Key::Down);
    let drawn = menu.render();
    let lines: Vec<&str> = drawn.split("\r\n").filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 2);
    assert!(lines[0].starts_with("  1 first"));
    assert!(lines[1].starts_with("\x1b[1m> 2 second"));
}
