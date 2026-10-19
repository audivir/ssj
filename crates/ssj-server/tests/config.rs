use std::ffi::OsString;
use std::time::Duration;

use ssj_server::cli::{Args, parse};
use ssj_server::config::Config;

fn args(words: &[&str]) -> Args {
    parse(words.iter().map(OsString::from)).unwrap()
}

#[test]
fn defaults_apply_without_a_file() {
    let config = Config::from_text("# nothing\n").unwrap();
    let mut a = args(&[]);
    a.apply_config(&config);
    assert_eq!(a, args(&[]));
    assert!(config.accept_env().matches("TERM") && !config.accept_env().matches("PATH"));
}

#[test]
fn every_key_is_read() {
    let config = Config::from_text(
        "port_range = \"50000:50010\"\n\
         detached_timeout = 3600\n\
         max_detached = 86400\n\
         away_after = 30\n\
         slow_client = \"wait\"\n\
         compress = \"off\"\n\
         congestion = \"cubic\"\n\
         live_backlog = 300000\n\
         shell = \"/bin/zsh\"\n\
         accept_env = [\"TERM\", \"EDITOR\"]\n",
    )
    .unwrap();
    let mut a = args(&[]);
    a.apply_config(&config);
    assert_eq!(a.port_range, 50000..=50010);
    assert_eq!(a.detached_timeout, Some(Duration::from_secs(3600)));
    assert_eq!(a.max_detached, Some(Duration::from_secs(86400)));
    assert_eq!(a.away_after, Duration::from_secs(30));
    assert_eq!(a.slow_client, ssj_server::session::SlowClient::Wait);
    assert_eq!(a.live_backlog, 300_000);
    assert!(!a.compress);
    assert_eq!(a.congestion, ssj_net::Congestion::Cubic);
    assert_eq!(a.shell.as_deref(), Some(std::path::Path::new("/bin/zsh")));
    assert!(config.accept_env().matches("EDITOR") && !config.accept_env().matches("LANG"));
}

#[test]
fn none_and_zero_turn_a_timeout_off() {
    for text in ["detached_timeout = \"none\"", "detached_timeout = 0"] {
        let mut a = args(&[]);
        a.apply_config(&Config::from_text(text).unwrap());
        assert_eq!(a.detached_timeout, None, "{text}");
    }
}

#[test]
fn flags_win_over_the_file() {
    let config = Config::from_text(
        "port_range = \"50000:50010\"\ndetached_timeout = 3600\nmax_detached = 100\naway_after = 30\nshell = \"/bin/zsh\"\n",
    )
    .unwrap();
    let mut a = args(&[
        "--port-range",
        "55000:55001",
        "--detached-timeout",
        "none",
        "--max-detached",
        "none",
        "--away-after",
        "5",
        "--shell",
        "/bin/sh",
    ]);
    a.apply_config(&config);
    assert_eq!(a.port_range, 55000..=55001);
    assert_eq!((a.detached_timeout, a.max_detached), (None, None));
    assert_eq!(a.away_after, Duration::from_secs(5));
    assert_eq!(a.shell.as_deref(), Some(std::path::Path::new("/bin/sh")));
}

#[test]
fn accept_env_cannot_be_set_on_the_command_line() {
    assert!(parse(["--accept-env", "X"].iter().map(OsString::from)).is_err());
}

#[test]
fn mistakes_are_errors_that_name_the_problem() {
    for (text, expected) in [
        ("port_rang = \"1:2\"", "unknown key"),
        ("port_range = \"1000:2000\"", "port_range: ports below 1024"),
        (
            "away_after = 0",
            "away_after: expected a number of seconds above 0",
        ),
        ("port_range = 5", "line 1: port_range must be a string"),
        ("detached_timeout = -5", "must not be negative"),
        ("detached_timeout = \"soon\"", "number of seconds"),
        ("max_detached = true", "line 1: max_detached must be"),
        (
            "slow_client = \"block\"",
            "slow_client: expected drop or wait",
        ),
        ("live_backlog = 10", "live_backlog: must be between"),
        ("compress = \"zlib\"", "compress: expected auto or off"),
        (
            "congestion = \"reno\"",
            "congestion: expected auto, bbr, or cubic",
        ),
        ("shell = \"zsh\"", "absolute path"),
        ("accept_env = [\"A B\"]", "accept_env: InvalidCharacter"),
        ("accept_env = \"TERM\"", "line 1: accept_env must be a list"),
    ] {
        let error = Config::from_text(text).unwrap_err();
        assert!(error.contains(expected), "{text:?}: {error}");
    }
}

// The daemon starts with the values of the bootstrap process on its command line.
#[test]
fn the_daemon_receives_the_resolved_values() {
    let mut a = args(&[]);
    a.apply_config(&Config::from_text("max_detached = 500\ndetached_timeout = 60\n").unwrap());
    let child = parse(a.to_child_args()).unwrap();
    assert_eq!(
        (child.detached_timeout, child.max_detached),
        (
            Some(Duration::from_secs(60)),
            Some(Duration::from_secs(500))
        )
    );
}

#[test]
fn replay_capacity_comes_from_the_file_and_a_flag_wins() {
    let mut a = args(&[]);
    assert_eq!(a.replay_capacity, 4 << 20, "the default is four mebibytes");

    let config = Config::from_text("replay_capacity = 1048576\n").unwrap();
    a.apply_config(&config);
    assert_eq!(a.replay_capacity, 1 << 20);

    let mut flagged = args(&["--replay-capacity", "262144"]);
    flagged.apply_config(&config);
    assert_eq!(flagged.replay_capacity, 262_144);
}

#[test]
fn replay_capacity_is_limited_to_a_sensible_range() {
    for ok in ["65536", "4194304", "1073741824"] {
        assert!(
            parse(["--replay-capacity", ok].iter().map(OsString::from)).is_ok(),
            "{ok}"
        );
        assert!(
            Config::from_text(&format!("replay_capacity = {ok}")).is_ok(),
            "{ok}"
        );
    }
    for bad in ["0", "65535", "1073741825", "-1", "big", "1.5", ""] {
        assert!(
            parse(["--replay-capacity", bad].iter().map(OsString::from)).is_err(),
            "{bad:?}"
        );
    }
    for bad in [
        "replay_capacity = 65535",
        "replay_capacity = -1",
        "replay_capacity = \"big\"",
        "replay_capacity = 1073741825",
    ] {
        assert!(Config::from_text(bad).is_err(), "{bad}");
    }
    assert!(
        Config::from_text("replay_capacity = 1")
            .unwrap_err()
            .contains("must be between")
    );
}

#[test]
fn the_daemon_receives_the_replay_capacity() {
    let mut a = args(&[]);
    a.apply_config(&Config::from_text("replay_capacity = 2097152\n").unwrap());
    let child = parse(a.to_child_args()).unwrap();
    assert_eq!(child.replay_capacity, 2 << 20);
}
