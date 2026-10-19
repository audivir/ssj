use std::ffi::OsString;
use std::time::Duration;

use ssj_server::cli::{Args, parse, parse_port_range, parse_timeout};

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
fn defaults_are_conservative_and_unknown_arguments_fail() {
    let defaults = args(&[]).unwrap();
    assert_eq!(defaults.port_range, 60000..=61000);
    assert_eq!(defaults.detached_timeout, None);

    assert!(args(&["--bogus"]).is_err());
    assert!(args(&["--port-range"]).is_err());
}

#[test]
fn child_arguments_keep_any_port() {
    let original = args(&["--port-range", "0"]).unwrap();
    assert_eq!(parse(original.to_child_args()).unwrap().port_range, 0..=0);
}

#[test]
fn the_session_id_travels_to_the_daemon_and_is_checked() {
    let id = "abcdef0123456789";
    let original = args(&["--session-id", id]).unwrap();
    assert_eq!(original.session_id.as_ref().map(|i| i.as_str()), Some(id));
    let child = parse(original.to_child_args()).unwrap();
    assert_eq!(child.session_id, original.session_id);
    assert!(args(&["--session-id", "no good"]).is_err());
    assert!(args(&["--session-id"]).is_err());
    assert!(args(&[]).unwrap().session_id.is_none());
}

// The daemon must behave exactly like the bootstrap process that started it.
#[test]
fn child_arguments_carry_every_setting() {
    let original = args(&[
        "--port-range",
        "50000:50010",
        "--detached-timeout",
        "none",
        "--away-after",
        "7",
        "--slow-client",
        "wait",
        "--live-backlog",
        "200000",
        "--compress",
        "off",
        "--congestion",
        "cubic",
        "--shell",
        "/bin/zsh",
        "--shell-arg",
        "-l",
        "--shell-arg",
        "-i",
    ])
    .unwrap();
    let child = parse(original.to_child_args()).unwrap();
    assert!(child.daemon_child);
    assert_eq!(
        Args {
            daemon_child: false,
            explicit: original.explicit,
            ..child
        },
        original
    );
}

#[test]
fn names_and_selectors_are_checked_and_carried_to_the_daemon() {
    let named = args(&["--name", "work", "--reuse"]).unwrap();
    assert_eq!(named.name.as_ref().map(|n| n.as_str()), Some("work"));
    assert!(named.reuse);
    let child = parse(named.to_child_args()).unwrap();
    assert_eq!(child.name, named.name);

    for bad in ["", "-x", ".x", "a b", "a/b", &"n".repeat(33)] {
        assert!(args(&["--name", bad]).is_err(), "{bad:?}");
    }
    assert!(args(&["--name"]).is_err());

    // a selector is an id or a name
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
fn the_job_does_not_wait_for_a_slow_client_unless_asked() {
    use ssj_server::session::SlowClient;

    let plain = args(&[]).unwrap();
    assert_eq!(plain.slow_client, SlowClient::Drop);
    assert_eq!(plain.live_backlog, 1 << 20);
    assert_eq!(
        args(&["--slow-client", "wait"]).unwrap().slow_client,
        SlowClient::Wait
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
fn congestion_control_is_bbr_unless_cubic_is_asked_for() {
    use ssj_net::Congestion;
    assert_eq!(args(&[]).unwrap().congestion, Congestion::Bbr);
    assert_eq!(
        args(&["--congestion", "cubic"]).unwrap().congestion,
        Congestion::Cubic
    );
    assert!(args(&["--congestion", "reno"]).is_err());
    assert!(args(&["--congestion"]).is_err());
}

#[test]
fn compression_is_automatic_unless_turned_off() {
    assert!(!args(&[]).unwrap().compress, "off unless asked for");
    assert!(args(&["--compress", "auto"]).unwrap().compress);
    assert!(!args(&["--compress", "off"]).unwrap().compress);
    assert!(args(&["--compress", "on"]).is_err());
    assert!(args(&["--compress"]).is_err());
}
