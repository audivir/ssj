use ssj_proto::destination::{Destination, DestinationError, parse};

fn dest(user: Option<&str>, host: &str, port: Option<u16>) -> Destination {
    Destination {
        user: user.map(str::to_owned),
        host: host.to_owned(),
        port,
    }
}

#[test]
fn the_usual_forms_are_understood() {
    for (text, expected) in [
        ("host", dest(None, "host", None)),
        ("tim@host", dest(Some("tim"), "host", None)),
        (
            "tim@host.example.org",
            dest(Some("tim"), "host.example.org", None),
        ),
        ("host:2222", dest(None, "host", Some(2222))),
        ("tim@host:2222", dest(Some("tim"), "host", Some(2222))),
        ("192.0.2.7", dest(None, "192.0.2.7", None)),
        ("tim@192.0.2.7:22", dest(Some("tim"), "192.0.2.7", Some(22))),
        ("my_alias-1", dest(None, "my_alias-1", None)),
        ("first.last@host", dest(Some("first.last"), "host", None)),
        ("a+b@host", dest(Some("a+b"), "host", None)),
        // URI
        ("ssh://host", dest(None, "host", None)),
        ("ssh://tim@host:2222", dest(Some("tim"), "host", Some(2222))),
        ("ssh://tim@host/", dest(Some("tim"), "host", None)),
        // IPv6: with brackets, with a port, and bare.
        ("[::1]", dest(None, "::1", None)),
        ("[2001:db8::1]:2222", dest(None, "2001:db8::1", Some(2222))),
        ("tim@[2001:db8::1]", dest(Some("tim"), "2001:db8::1", None)),
        ("::1", dest(None, "::1", None)),
        ("tim@2001:db8::1", dest(Some("tim"), "2001:db8::1", None)),
        ("fe80::1%en0", dest(None, "fe80::1%en0", None)),
    ] {
        assert_eq!(parse(text), Ok(expected), "{text:?}");
    }
}

#[test]
fn the_argument_for_ssh_has_no_port_and_no_brackets() {
    for (text, target) in [
        ("tim@host:2222", "tim@host"),
        ("host", "host"),
        ("[2001:db8::1]:22", "2001:db8::1"),
        ("tim@[::1]", "tim@::1"),
        ("ssh://tim@host:22", "tim@host"),
    ] {
        assert_eq!(parse(text).unwrap().target(), target, "{text:?}");
    }
}

#[test]
fn what_could_be_an_option_or_a_command_is_refused() {
    for (text, error) in [
        ("", DestinationError::Empty),
        ("-oProxyCommand=id", DestinationError::BadHost),
        ("-host", DestinationError::BadHost),
        ("user@-host", DestinationError::BadHost),
        ("-user@host", DestinationError::BadUser),
        ("ssh://-host", DestinationError::BadHost),
        ("[-::1]", DestinationError::BadHost),
        (".host", DestinationError::BadHost),
        ("host name", DestinationError::BadHost),
        ("host\n", DestinationError::BadHost),
        ("host;id", DestinationError::BadHost),
        ("$(id)", DestinationError::BadHost),
        ("`id`", DestinationError::BadHost),
        ("host'quote", DestinationError::BadHost),
        ("ho/st", DestinationError::BadHost),
        ("host\0", DestinationError::BadHost),
        ("h\u{f6}st", DestinationError::BadHost),
        ("tim@@host", DestinationError::BadHost),
        ("a b@host", DestinationError::BadUser),
        ("@host", DestinationError::BadUser),
        ("tim@", DestinationError::BadHost),
        ("tim@host@other", DestinationError::BadHost),
        ("ssh://", DestinationError::BadHost),
        ("[::1", DestinationError::BadHost),
        ("[::1]x", DestinationError::BadHost),
        ("[host]", DestinationError::BadHost),
        ("[1:2:3]", DestinationError::BadHost),
        (":::", DestinationError::BadHost),
        ("fe80::1%", DestinationError::BadHost),
        ("fe80::1%-x", DestinationError::BadHost),
        (":22", DestinationError::BadHost),
        ("host:", DestinationError::BadPort),
        ("host:0", DestinationError::BadPort),
        ("host:022", DestinationError::BadPort),
        ("host:65536", DestinationError::BadPort),
        ("host:+22", DestinationError::BadPort),
        ("host:2 2", DestinationError::BadPort),
        ("host:x", DestinationError::BadPort),
        ("[::1]:0", DestinationError::BadPort),
    ] {
        assert_eq!(parse(text), Err(error), "{text:?}");
    }
    assert!(parse(&"a".repeat(254)).is_err());
    assert!(parse(&"a".repeat(253)).is_ok());
    assert!(parse(&format!("{}@host", "u".repeat(65))).is_err());
}

#[test]
fn the_canonical_form_parses_back() {
    for text in [
        "host",
        "tim@host:2222",
        "[::1]",
        "tim@[2001:db8::1]:22",
        "2001:db8::1",
        "ssh://tim@host",
    ] {
        let parsed = parse(text).unwrap();
        assert_eq!(parse(&parsed.to_string()), Ok(parsed), "{text:?}");
    }
}
