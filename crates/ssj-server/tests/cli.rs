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
    for bad in [
        "",
        "1000",
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
    assert_eq!(
        defaults.detached_timeout,
        Some(Duration::from_secs(7 * 24 * 3600))
    );
    assert!(!defaults.allow_root);

    assert!(args(&["--bogus"]).is_err());
    assert!(args(&["--port-range"]).is_err());
}

// The daemon must behave exactly like the bootstrap process that started it.
#[test]
fn child_arguments_carry_every_setting() {
    let original = args(&[
        "--port-range",
        "50000:50010",
        "--detached-timeout",
        "none",
        "--shell",
        "/bin/zsh",
        "--shell-arg",
        "-l",
        "--shell-arg",
        "-i",
        "--allow-root",
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
