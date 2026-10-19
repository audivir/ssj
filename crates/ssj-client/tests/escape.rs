use ssj_client::escape::{Action, EscapeFilter};

fn send(bytes: &[u8]) -> Action {
    Action::Send(bytes.to_vec())
}

/// Feeds the chunks like the client does: after a detach nothing is read anymore.
fn feed_all(filter: &mut EscapeFilter, chunks: &[&[u8]]) -> Vec<Action> {
    let mut out = Vec::new();
    for chunk in chunks {
        out.extend(filter.feed(chunk));
        if out.contains(&Action::Detach) {
            break;
        }
    }
    out
}

/// Merges neighbouring sends, so that the chunking of the input does not show.
fn merged(actions: Vec<Action>) -> Vec<Action> {
    let mut out: Vec<Action> = Vec::new();
    for action in actions {
        match (out.last_mut(), action) {
            (Some(Action::Send(last)), Action::Send(more)) => last.extend(more),
            (_, action) => out.push(action),
        }
    }
    out
}

#[test]
fn escape_needs_the_start_of_a_line() {
    let cases: Vec<(&[u8], Vec<Action>)> = vec![
        // at the start of the session
        (b"~.", vec![Action::Detach]),
        (b"~?", vec![Action::Help]),
        (b"~~", vec![send(b"~")]),
        (b"~x", vec![send(b"~x")]),
        // after a newline of either kind
        (b"ls\r~.", vec![send(b"ls\r"), Action::Detach]),
        (b"ls\n~.", vec![send(b"ls\n"), Action::Detach]),
        // in the middle of a line the tilde is just a tilde
        (b"ls ~.", vec![send(b"ls ~.")]),
        (b"a~.", vec![send(b"a~.")]),
        // a literal tilde ends the line start
        (b"~~.", vec![send(b"~.")]),
        // after the help the line start is over as well
        (b"~?~.", vec![Action::Help, send(b"~.")]),
        // an unknown character is passed on together with the tilde
        (b"~\r~.", vec![send(b"~\r"), Action::Detach]),
        // nothing after a detach is looked at
        (b"~.ls\r", vec![Action::Detach]),
        (b"", vec![]),
    ];
    for (typed, expected) in cases {
        let mut filter = EscapeFilter::new(Some(b'~'));
        assert_eq!(
            merged(filter.feed(typed)),
            expected,
            "{:?}",
            String::from_utf8_lossy(typed)
        );
    }
}

#[test]
fn chunking_of_the_input_does_not_change_the_result() {
    let typed: &[u8] = b"ls\r~~x\r~?echo ~. a\r~.rest";
    let whole = merged(EscapeFilter::new(Some(b'~')).feed(typed));

    for size in [1, 2, 3, 5, 7] {
        let chunks: Vec<&[u8]> = typed.chunks(size).collect();
        let mut filter = EscapeFilter::new(Some(b'~'));
        assert_eq!(
            merged(feed_all(&mut filter, &chunks)),
            whole,
            "chunks of {size}"
        );
    }
}

#[test]
fn another_escape_character_works_and_none_turns_it_off() {
    let mut filter = EscapeFilter::new(Some(b'#'));
    assert_eq!(filter.feed(b"~."), vec![send(b"~.")]);
    assert_eq!(filter.feed(b"\r#."), vec![send(b"\r"), Action::Detach]);

    let mut off = EscapeFilter::new(None);
    assert_eq!(off.feed(b"~."), vec![send(b"~.")]);
}

#[test]
fn the_detach_key_detaches_anywhere_and_keeps_what_came_before() {
    let mut filter = EscapeFilter::new(Some(b'~')).with_detach_key(Some(0x1d));
    // in the middle of a line, which the escape character cannot do
    assert_eq!(
        filter.feed(b"ls -l\x1dmore"),
        vec![send(b"ls -l"), Action::Detach]
    );

    // also with the escape character off, and when the key comes first
    let mut only_key = EscapeFilter::new(None).with_detach_key(Some(0x1d));
    assert_eq!(only_key.feed(b"abc"), vec![send(b"abc")]);
    assert_eq!(only_key.feed(b"\x1d"), vec![Action::Detach]);

    // the escape character still works next to it
    let mut both = EscapeFilter::new(Some(b'~')).with_detach_key(Some(0x1d));
    assert_eq!(both.feed(b"~."), vec![Action::Detach]);

    // without a key every byte is for the shell
    let mut none = EscapeFilter::new(Some(b'~'));
    assert_eq!(none.feed(b"a\x1db"), vec![send(b"a\x1db")]);
}

#[test]
fn the_help_names_the_detach_key_when_there_is_one() {
    use ssj_client::escape::help;
    assert!(!help(None).contains("anywhere"));
    assert!(help(Some(0x1d)).contains("^]  detach, anywhere"));
}

#[test]
fn inside_ssh_the_help_says_that_ssh_reads_the_escape_first() {
    use ssj_client::escape::help_in;
    assert!(help_in(None, true).contains("type ~~. instead"));
    assert!(!help_in(None, false).contains("ssh"));
}
