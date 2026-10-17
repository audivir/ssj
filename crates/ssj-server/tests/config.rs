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
         shell = \"/bin/zsh\"\n\
         accept_env = [\"TERM\", \"EDITOR\"]\n",
    )
    .unwrap();
    let mut a = args(&[]);
    a.apply_config(&config);
    assert_eq!(a.port_range, 50000..=50010);
    assert_eq!(a.detached_timeout, Some(Duration::from_secs(3600)));
    assert_eq!(a.max_detached, Some(Duration::from_secs(86400)));
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
        "port_range = \"50000:50010\"\ndetached_timeout = 3600\nmax_detached = 100\nshell = \"/bin/zsh\"\n",
    )
    .unwrap();
    let mut a = args(&[
        "--port-range",
        "55000:55001",
        "--detached-timeout",
        "none",
        "--max-detached",
        "none",
        "--shell",
        "/bin/sh",
    ]);
    a.apply_config(&config);
    assert_eq!(a.port_range, 55000..=55001);
    assert_eq!((a.detached_timeout, a.max_detached), (None, None));
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
        ("port_range = 5", "line 1: port_range must be a string"),
        ("detached_timeout = -5", "must not be negative"),
        ("detached_timeout = \"soon\"", "number of seconds"),
        ("max_detached = true", "line 1: max_detached must be"),
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
