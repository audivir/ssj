use std::ffi::OsString;
use std::time::Duration;

use ssj_server::cli::{Settings, parse};
use ssj_server::config::Config;
use ssj_server::session::SlowClient;

fn flags(words: &[&str]) -> Settings {
    parse(words.iter().map(OsString::from)).unwrap().settings
}

// the defaults are conservative: no compression, and the job never waits for a slow client.
#[test]
fn defaults_apply_without_a_file() {
    let config = Config::from_text("# nothing\n").unwrap();
    assert_eq!(config.port_range, 60000..=61000);
    assert_eq!((config.detached_timeout, config.max_detached), (None, None));
    assert_eq!(config.replay_capacity, 4 << 20);
    assert_eq!(config.slow_client, SlowClient::Drop);
    assert_eq!(config.live_backlog, 1 << 20);
    assert!(!config.compress);
    assert_eq!(config.congestion, ssj_net::Congestion::Auto);
    assert!(config.accept_env.matches("TERM") && !config.accept_env.matches("PATH"));
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
    let a = config.clone().with(&flags(&[]));
    assert_eq!(a.port_range, 50000..=50010);
    assert_eq!(a.detached_timeout, Some(Duration::from_secs(3600)));
    assert_eq!(a.max_detached, Some(Duration::from_secs(86400)));
    assert_eq!(a.away_after, Duration::from_secs(30));
    assert_eq!(a.slow_client, SlowClient::Wait);
    assert_eq!(a.live_backlog, 300_000);
    assert!(!a.compress);
    assert_eq!(a.congestion, ssj_net::Congestion::Cubic);
    assert_eq!(a.shell.as_deref(), Some(std::path::Path::new("/bin/zsh")));
    assert!(config.accept_env.matches("EDITOR") && !config.accept_env.matches("LANG"));
}

#[test]
fn none_and_zero_turn_a_timeout_off() {
    for text in ["detached_timeout = \"none\"", "detached_timeout = 0"] {
        let a = Config::from_text(text).unwrap();
        assert_eq!(a.detached_timeout, None, "{text}");
    }
}

#[test]
fn flags_win_over_the_file() {
    let config = Config::from_text(
        "port_range = \"50000:50010\"\ndetached_timeout = 3600\nmax_detached = 100\naway_after = 30\nshell = \"/bin/zsh\"\n",
    )
    .unwrap();
    let a = config.with(&flags(&[
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
    ]));
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
        ("port_rang = \"1:2\"", "unknown field `port_rang`"),
        ("port_range = \"1000:2000\"", "port_range: ports below 1024"),
        (
            "away_after = 0",
            "away_after: expected a number of seconds above 0",
        ),
        ("port_range = 5", "line 1: port_range: invalid type"),
        ("detached_timeout = -5", "expected a number of seconds"),
        ("detached_timeout = \"soon\"", "number of seconds"),
        ("max_detached = true", "line 1: max_detached: invalid type"),
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
        ("accept_env = \"TERM\"", "line 1: accept_env: invalid type"),
    ] {
        let error = Config::from_text(text).unwrap_err();
        assert!(error.contains(expected), "{text:?}: {error}");
    }
}

#[test]
fn replay_capacity_comes_from_the_file_and_a_flag_wins() {
    let config = Config::from_text("replay_capacity = 1048576\n").unwrap();
    assert_eq!(config.clone().with(&flags(&[])).replay_capacity, 1 << 20);
    let flagged = config.with(&flags(&["--replay-capacity", "262144"]));
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
