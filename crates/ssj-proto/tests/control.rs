use ssj_proto::bootstrap::Connect;
use ssj_proto::control::{
    ControlError, Reply, Request, SessionInfo, SessionState, format_reply, format_request,
    parse_reply, parse_request,
};
use ssj_proto::{ReplayBuffer, SessionId};

const HEX: &str = "030b131b232b333b434b535b636b737b838b939ba3abb3bbc3cbd3dbe3ebf3fb";

fn fingerprint() -> [u8; 32] {
    let mut bytes = [0u8; 32];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = (i * 8 + 3) as u8;
    }
    bytes
}

fn info(state: SessionState) -> SessionInfo {
    SessionInfo {
        id: SessionId::parse("abcdef012345").unwrap(),
        state,
        age_secs: 3600,
        protocol: 1,
    }
}

#[test]
fn requests_have_a_fixed_shape_and_survive_a_round_trip() {
    assert_eq!(format_request(&Request::Status), "STATUS\n");
    assert_eq!(format_request(&Request::Kill), "KILL\n");
    assert_eq!(
        format_request(&Request::Attach(fingerprint())),
        format!("ATTACH {HEX}\n")
    );
    for request in [
        Request::Status,
        Request::Kill,
        Request::Attach(fingerprint()),
    ] {
        assert_eq!(
            parse_request(format_request(&request).as_bytes()),
            Ok(request)
        );
        assert_eq!(
            parse_request(format_request(&request).trim_end().as_bytes()),
            Ok(request)
        );
    }
}

#[test]
fn malformed_requests_are_refused() {
    let upper = HEX.to_uppercase();
    for bad in [
        String::new(),
        "status\n".into(),
        "STATUS \n".into(),
        "STATUS extra\n".into(),
        "KILL 1\n".into(),
        "ATTACH\n".into(),
        "ATTACH \n".into(),
        format!("ATTACH {upper}\n"),
        format!("ATTACH {}\n", &HEX[1..]),
        format!("ATTACH {HEX}00\n"),
        format!("ATTACH  {HEX}\n"),
        format!("SSJ PIN {HEX}\n"),
        "DETACH\n".into(),
    ] {
        assert_eq!(
            parse_request(bad.as_bytes()),
            Err(ControlError::Malformed),
            "{bad:?}"
        );
    }
}

#[test]
fn replies_survive_a_round_trip() {
    let replies = [
        Reply::Connect(Connect {
            port: 60123,
            fingerprint: fingerprint(),
        }),
        Reply::Info(info(SessionState::Connected)),
        Reply::Info(info(SessionState::Disconnected)),
        Reply::Info(info(SessionState::Detached)),
        Reply::Ok,
        Reply::Error("no free UDP port between 60000 and 61000".to_owned()),
    ];
    for reply in replies {
        assert_eq!(
            parse_reply(format_reply(&reply).as_bytes()),
            Ok(reply.clone()),
            "{reply:?}"
        );
    }
}

#[test]
fn malformed_replies_are_refused() {
    for bad in [
        "",
        "ok\n",
        "OK extra\n",
        "ERR \u{1b}[2J\n",
        "ERR caf\u{e9}\n",
        "SSJ SESSION abcdef012345 connected 5\n",
        "SSJ SESSION abcdef012345 connected 5 1 extra\n",
        "SSJ SESSION ABCDEF012345 connected 5 1\n",
        "SSJ SESSION abcdef012345 sleeping 5 1\n",
        "SSJ SESSION abcdef012345 connected -5 1\n",
        "SSJ SESSION abcdef012345 connected 05 1\n",
        "SSJ SESSION abcdef012345 connected 99999999999 1\n",
        "SSJ SESSION ../../etc/passwd connected 5 1\n",
        "SSJ CONNECT 0 00\n",
        "OK\nOK\n",
        "OK\r\n",
    ] {
        assert_eq!(
            parse_reply(bad.as_bytes()),
            Err(ControlError::Malformed),
            "{bad:?}"
        );
    }
}

// Text from the server is shown to the user, so it must not carry control characters.
#[test]
fn error_text_is_reduced_to_printable_ascii_and_cut() {
    let line = format_reply(&Reply::Error(format!(
        "bad\u{1b}[2J\nline\u{e9}{}",
        "x".repeat(500)
    )));
    assert_eq!(line.matches('\n').count(), 1);
    assert!(line.len() <= 205, "{}", line.len());
    assert!(
        matches!(parse_reply(line.as_bytes()), Ok(Reply::Error(text)) if text.starts_with("bad[2Jline"))
    );
}

#[test]
fn buffer_can_start_in_the_middle_of_a_stream() {
    let mut buf = ReplayBuffer::starting_at(16, 1000);
    assert_eq!((buf.start(), buf.end()), (1000, 1000));
    buf.push(b"abc");
    assert_eq!(buf.end(), 1003);
    assert_eq!(buf.read_from(1001, 10).unwrap(), b"bc");
    buf.ack(1002).unwrap();
    assert_eq!(buf.start(), 1002);
}

#[test]
fn a_reply_with_more_than_one_line_is_refused() {
    let connect = format_reply(&Reply::Connect(Connect {
        port: 1234,
        fingerprint: fingerprint(),
    }));
    assert!(parse_reply(connect.as_bytes()).is_ok());
    assert_eq!(
        parse_reply(format!("{connect}extra\n").as_bytes()),
        Err(ControlError::Malformed)
    );
    assert_eq!(
        parse_reply(format!("{connect}{connect}").as_bytes()),
        Err(ControlError::Malformed)
    );
    assert_eq!(
        parse_reply(format!("noise\n{connect}").as_bytes()),
        Err(ControlError::Malformed)
    );
}
