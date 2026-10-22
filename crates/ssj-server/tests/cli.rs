use std::ffi::OsString;
use std::time::Duration;

use ssj_server::cli::{Args, Settings, parse, parse_port_range, parse_timeout};

fn args(list: &[&str]) -> Result<Args, String> {
    parse(list.iter().map(OsString::from))
}

#[test]
fn port_ranges_must_be_unprivileged_and_ordered() {
    assert_eq!(parse_port_range("60000:61000"), Ok(60000..=61000));
    assert_eq!(parse_port_range("1024:1024"), Ok(1024..=1024));
    // any port
    assert_eq!(parse_port_range("0"), Ok(0..=0));
    for bad in [
        "",
        "1000",
        "00",
        "0:0",
        "0:2000",
        "1023:2000",
        "2000:1999",
        "a:b",
        "1:",
        ":2000",
        "70000:70001",
        "2000:3000:4000",
    ] {
        assert!(parse_port_range(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn timeouts_are_seconds_or_none() {
    assert_eq!(parse_timeout("90"), Ok(Some(Duration::from_secs(90))));
    assert_eq!(parse_timeout("none"), Ok(None));
    assert_eq!(parse_timeout("0"), Ok(None));
    for bad in ["", "-1", "1.5", "soon"] {
        assert!(parse_timeout(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn without_flags_the_file_decides_and_unknown_arguments_fail() {
    assert_eq!(args(&[]).unwrap().settings, Settings::default());
    assert!(args(&["--bogus"]).is_err());
    assert!(args(&["--port-range"]).is_err());
    assert_eq!(
        args(&["--port-range", "0"]).unwrap().settings.port_range,
        Some(0..=0)
    );
}

// the daemon gets the arguments of the bootstrap process, and after them its own.
#[test]
fn the_daemon_takes_the_arguments_of_the_bootstrap_and_its_session_id() {
    let id = "abcdef0123456789";
    let words = [
        "--port-range",
        "50000:50010",
        "--detached-timeout",
        "none",
        "--shell-arg",
        "-l",
        "--session-id",
        "zzzzzzzzzzzzzzzz",
    ];
    let original = args(&words).unwrap();
    let child = args(&[&words[..], &["--daemon-child", "--session-id", id]].concat()).unwrap();
    assert!(child.daemon_child);
    assert_eq!(child.session_id.as_ref().map(|i| i.as_str()), Some(id));
    assert_eq!(
        Args {
            daemon_child: false,
            session_id: original.session_id.clone(),
            ..child
        },
        original
    );
    assert!(args(&["--session-id", "no good"]).is_err());
    assert!(args(&["--session-id"]).is_err());
    assert!(args(&[]).unwrap().session_id.is_none());
}

#[test]
fn names_and_selectors_are_checked() {
    let named = args(&["--name", "work", "--reuse"]).unwrap();
    assert_eq!(named.name.as_ref().map(|n| n.as_str()), Some("work"));
    assert!(named.reuse);

    for bad in ["", "-x", ".x", "a b", "a/b", &"n".repeat(33)] {
        assert!(args(&["--name", bad]).is_err(), "{bad:?}");
    }
    assert!(args(&["--name"]).is_err());

    // a selector is an id or a name.
    assert_eq!(
        args(&["--attach", "work"]).unwrap().mode,
        ssj_server::cli::Mode::Attach(Some("work".to_owned()))
    );
    assert_eq!(
        args(&["--kill", "abcdef012345"]).unwrap().mode,
        ssj_server::cli::Mode::Kill(vec!["abcdef012345".to_owned()])
    );
    assert_eq!(
        args(&["--kill", "a1", "work", "--choose"]).unwrap().mode,
        ssj_server::cli::Mode::Kill(vec!["a1".to_owned(), "work".to_owned()])
    );
    assert_eq!(
        args(&["--kill"]).unwrap().mode,
        ssj_server::cli::Mode::Kill(Vec::new())
    );
    assert!(args(&["--kill", "a b"]).is_err());
    assert_eq!(
        args(&["--stdio", "work"]).unwrap().mode,
        ssj_server::cli::Mode::Stdio(Some("work".to_owned()))
    );
}

#[test]
fn the_way_with_a_slow_client_and_its_backlog_are_checked() {
    use ssj_server::session::SlowClient;

    assert_eq!(
        args(&["--slow-client", "wait"])
            .unwrap()
            .settings
            .slow_client,
        Some(SlowClient::Wait)
    );
    assert_eq!(
        args(&["--live-backlog", "200000"])
            .unwrap()
            .settings
            .live_backlog,
        Some(200_000)
    );
    for bad in [
        vec!["--slow-client", "block"],
        vec!["--slow-client"],
        vec!["--live-backlog", "1000"],
        vec!["--live-backlog", "x"],
        vec!["--live-backlog", "99999999999"],
    ] {
        assert!(args(&bad).is_err(), "{bad:?}");
    }
}

#[test]
fn congestion_control_takes_a_known_way() {
    use ssj_net::Congestion;
    for (word, way) in [
        ("cubic", Congestion::Cubic),
        ("bbr", Congestion::Bbr),
        ("auto", Congestion::Auto),
    ] {
        assert_eq!(
            args(&["--congestion", word]).unwrap().settings.congestion,
            Some(way)
        );
    }
    assert!(args(&["--congestion", "reno"]).is_err());
    assert!(args(&["--congestion"]).is_err());
}

#[test]
fn compression_is_auto_or_off() {
    assert_eq!(
        args(&["--compress", "auto"]).unwrap().settings.compress,
        Some(ssj_net::Compress::Auto)
    );
    assert_eq!(
        args(&["--compress", "off"]).unwrap().settings.compress,
        Some(ssj_net::Compress::Off)
    );
    assert!(args(&["--compress", "on"]).is_err());
    assert!(args(&["--compress"]).is_err());
}
