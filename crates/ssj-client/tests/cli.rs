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

    let parsed = args(&[
        "--server-path",
        "/opt/it's/ssj-server",
        "--server-arg",
        "a b",
        "host",
    ])
    .unwrap();
    assert_eq!(
        remote_command(&parsed, &[]),
        r"'/opt/it'\''s/ssj-server' 'a b'"
    );
}

// The default command is run by the remote login shell like this, so the test uses a real shell.
#[test]
fn default_command_runs_the_installed_file_or_reports_it_missing() {
    use std::os::unix::fs::PermissionsExt;

    let data = std::path::PathBuf::from(format!("/tmp/ssjt-cli-{}", std::process::id()));
    let bin = data.join("ssj/bin");
    std::fs::create_dir_all(&bin).unwrap();
    let run = |args: &Args| {
        Command::new("sh")
            .arg("-c")
            .arg(remote_command(args, &["--attach".to_owned()]))
            .env("XDG_DATA_HOME", &data)
            .output()
            .unwrap()
    };
    let mut parsed = args(&["--server-arg", "a b; touch /tmp/ssj-injected", "host"]).unwrap();
    assert_eq!(parsed.server_path, ServerPath::Default);

    // nothing installed: a marker and a code that the client can tell apart from other failures
    let missing = run(&parsed);
    assert_eq!(missing.status.code(), Some(66));
    assert_eq!(String::from_utf8_lossy(&missing.stdout), "SSJ MISSING\n");

    // installed: the file runs, and every argument arrives as one word
    let file = bin.join("ssj-server-p1");
    std::fs::write(
        &file,
        "#!/bin/sh\nfor a in \"$@\"; do echo \"arg:$a\"; done\n",
    )
    .unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
    let ran = run(&parsed);
    assert_eq!(
        String::from_utf8_lossy(&ran.stdout),
        "arg:a b; touch /tmp/ssj-injected\narg:--attach\n"
    );
    assert!(!std::path::Path::new("/tmp/ssj-injected").exists());

    // a file that is not executable counts as missing
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(run(&parsed).status.code(), Some(66));

    // a path given by the user is used as it is, without the wrapper
    parsed.server_path = ServerPath::Custom("/nonexistent/ssj-server".to_owned());
    assert_eq!(run(&parsed).status.code(), Some(127));
    std::fs::remove_dir_all(&data).unwrap();
}

#[test]
fn subcommands_need_a_destination_after_them() {
    use ssj_client::cli::Command;
    use ssj_proto::SessionId;

    let id = SessionId::parse("abcdef012345").unwrap();
    let cases: Vec<(Vec<&str>, Command, &str)> = vec![
        (vec!["host"], Command::Connect, "host"),
        (vec!["list"], Command::Connect, "list"),
        (vec!["kill"], Command::Connect, "kill"),
        (vec!["list", "me@host"], Command::List, "me@host"),
        (vec!["attach", "host"], Command::Attach(None), "host"),
        (
            vec!["attach", "abcdef012345", "host"],
            Command::Attach(Some(id.clone())),
            "host",
        ),
        (vec!["kill", "host"], Command::Kill(None), "host"),
        (
            vec!["kill", "abcdef012345", "host"],
            Command::Kill(Some(id)),
            "host",
        ),
        (
            vec!["--escape-char", "none", "list", "host"],
            Command::List,
            "host",
        ),
    ];
    for (words, command, destination) in cases {
        let parsed = args(&words).unwrap_or_else(|e| panic!("{words:?}: {e}"));
        assert_eq!(parsed.command, command, "{words:?}");
        assert_eq!(parsed.destination, destination, "{words:?}");
    }
    for bad in [
        vec!["list", "a", "b"],
        vec!["kill", "../../x", "host"],
        vec!["kill", "ABCDEF012345", "host"],
        vec!["attach", "id", "a", "b"],
        vec!["host", "other"],
    ] {
        assert!(args(&bad).is_err(), "{bad:?}");
    }
}

#[test]
fn age_is_shown_in_the_largest_fitting_unit() {
    use ssj_client::run::format_age;

    for (seconds, text) in [
        (0, "0s"),
        (59, "59s"),
        (60, "1m"),
        (3599, "59m"),
        (3600, "1h"),
        (86399, "23h"),
        (86400, "1d"),
        (864000, "10d"),
    ] {
        assert_eq!(format_age(seconds), text);
    }
}
