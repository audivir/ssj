use std::process::Command;

use ssj_client::bootstrap::{remote_command, shell_quote};
use ssj_client::cli::{Args, ServerPath, parse};

fn args(list: &[&str]) -> Result<Args, String> {
    parse(list.iter().map(|s| s.to_string()))
}

#[test]
fn destination_and_options_are_parsed() {
    let parsed = args(&[
        "--port-range",
        "50000:50010",
        "--ssh-command",
        "ssh -J jump",
        "me@host",
    ])
    .unwrap();
    assert_eq!(parsed.destination, "me@host");
    assert_eq!(parsed.host(), "host");
    assert_eq!(parsed.ssh_command, ["ssh", "-J", "jump"]);
    assert_eq!(parsed.server_args, ["--port-range", "50000:50010"]);
    assert_eq!(parsed.server_path, ServerPath::Default);
    assert_eq!(args(&["host"]).unwrap().host(), "host");
}

#[test]
fn bad_command_lines_are_refused() {
    for bad in [
        vec![],
        vec!["a", "b"],
        vec!["--bogus", "host"],
        vec!["--ssh-command"],
        vec!["--ssh-command", "  ", "host"],
        vec!["host with space"],
        vec!["--send-env", "A-B", "host"],
    ] {
        assert!(args(&bad).is_err(), "{bad:?}");
    }
}

// The destination is passed after `--`, but an option-like word must not get that far at all.
#[test]
fn destination_that_looks_like_an_option_is_refused() {
    assert!(args(&["-oProxyCommand=touch /tmp/x"]).is_err());
}

// Whatever the user types for the server path or its arguments must reach the server
// unchanged and must never be run as shell syntax.
#[test]
fn remote_command_survives_the_remote_shell_unchanged() {
    let nasty = [
        "plain",
        "it's",
        "a b",
        "$(touch /tmp/ssj-injected)",
        "`touch /tmp/ssj-injected`",
        "x; touch /tmp/ssj-injected",
        "line\nbreak",
        "\"quoted\"",
        "back\\slash",
        "",
    ];
    for text in nasty {
        let output = Command::new("sh")
            .arg("-c")
            .arg(format!("printf '%s' {}", shell_quote(text)))
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&output.stdout), text, "{text:?}");
    }
    assert!(!std::path::Path::new("/tmp/ssj-injected").exists());

    let mut parsed = args(&[
        "--server-path",
        "/opt/it's/ssj-server",
        "--server-arg",
        "a b",
        "host",
    ])
    .unwrap();
    assert_eq!(remote_command(&parsed), r"'/opt/it'\''s/ssj-server' 'a b'");
    parsed.server_path = ServerPath::Default;
    assert!(
        remote_command(&parsed)
            .starts_with("\"${XDG_DATA_HOME:-$HOME/.local/share}/ssj/bin/ssj-server-p1\"")
    );
}
