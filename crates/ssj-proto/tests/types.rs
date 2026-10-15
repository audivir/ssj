use ssj_proto::{SessionId, WindowSize};

#[test]
fn accepts_lowercase_alphanumeric_ids() {
    assert_eq!(
        SessionId::parse("abcdef012345").unwrap().as_str(),
        "abcdef012345"
    );
}

// the id becomes part of socket paths and multiplexer commands
#[test]
fn rejects_ids_that_could_escape_a_path_or_a_command() {
    for id in [
        "",
        "short",
        "../../etc/passwd",
        "abcdef01/2345",
        "abcdef01.2345",
        "-abcdef012345",
        "abcdef 012345",
        "ABCDEF012345",
        "abcdef012345\n",
        &"a".repeat(65),
    ] {
        assert!(SessionId::parse(id).is_none(), "{id:?}");
    }
}

#[test]
fn window_size_is_limited_to_a_sane_range() {
    assert!(WindowSize::new(1, 1).is_some());
    assert!(WindowSize::new(WindowSize::MAX, WindowSize::MAX).is_some());
    for (rows, cols) in [(0, 80), (24, 0), (WindowSize::MAX + 1, 80), (24, u16::MAX)] {
        assert!(WindowSize::new(rows, cols).is_none(), "{rows}x{cols}");
    }
}
