use ssj_server::backend::SessionId;

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
