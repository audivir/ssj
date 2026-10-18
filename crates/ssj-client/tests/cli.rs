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
    let id = "abcdef012345".to_owned();
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
        // a name works where an id does
        (
            vec!["attach", "work", "host"],
            Command::Attach(Some("work".to_owned())),
            "host",
        ),
        (
            vec!["kill", "My-Session", "host"],
            Command::Kill(Some("My-Session".to_owned())),
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
        vec!["kill", "a b", "host"],
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

#[test]
fn the_transport_is_a_choice_with_a_default_and_a_file_setting() {
    use ssj_client::cli::{Transport, parse_transport};
    use ssj_client::config::Config;

    assert_eq!(args(&["host"]).unwrap().transport, Transport::Auto);
    for (word, transport) in [
        ("auto", Transport::Auto),
        ("udp", Transport::Udp),
        ("tcp", Transport::Tcp),
        ("stdio", Transport::Stdio),
    ] {
        assert_eq!(parse_transport(word), Ok(transport));
        let parsed = args(&["--transport", word, "host"]).unwrap();
        assert_eq!(parsed.transport, transport);
        assert!(parsed.explicit.transport);
    }
    for bad in ["", "UDP", "quic", "ssh"] {
        assert!(parse_transport(bad).is_err(), "{bad:?}");
    }
    assert!(args(&["--transport"]).is_err());
    assert!(args(&["--transport", "x", "host"]).is_err());

    // the file fills in what the command line leaves open
    let config = Config::from_text("transport = \"tcp\"\n").unwrap();
    let mut open = args(&["host"]).unwrap();
    open.apply_config(&config);
    assert_eq!(open.transport, Transport::Tcp);
    let mut set = args(&["--transport", "udp", "host"]).unwrap();
    set.apply_config(&config);
    assert_eq!(set.transport, Transport::Udp);
    assert!(Config::from_text("transport = \"x\"\n").is_err());
}

#[test]
fn the_destination_gives_the_argument_for_ssh_and_its_port() {
    for (text, target, port) in [
        ("host", "host", None),
        ("tim@host:2222", "tim@host", Some(2222)),
        ("ssh://tim@host:22", "tim@host", Some(22)),
        ("[2001:db8::1]:2200", "2001:db8::1", Some(2200)),
        ("tim@2001:db8::1", "tim@2001:db8::1", None),
    ] {
        let parsed = args(&[text]).unwrap();
        assert_eq!(parsed.destination, target, "{text}");
        assert_eq!(parsed.ssh_port, port, "{text}");
        assert_eq!(
            parsed.ssh_port_args(),
            port.map_or(vec![], |p| vec!["-p".to_owned(), p.to_string()]),
            "{text}"
        );
    }
    // the host part is what DNS gets
    assert_eq!(args(&["tim@host:2222"]).unwrap().host(), "host");
    assert_eq!(args(&["[::1]:22"]).unwrap().host(), "::1");
    // an option or a command in place of a destination
    for bad in ["-oProxyCommand=id", "host;id", "a b", "$(id)", "host:0", ""] {
        assert!(args(&[bad]).is_err(), "{bad:?}");
    }
    // an option is no destination even after the other arguments
    assert!(args(&["--", "-x"]).is_err());
}

#[test]
fn a_name_and_auto_attach_are_settings_with_flags_and_a_file_key() {
    use ssj_client::config::Config;

    let plain = args(&["host"]).unwrap();
    assert!(plain.name.is_none() && !plain.auto_attach);
    let named = args(&["--name", "work", "host"]).unwrap();
    assert_eq!(named.name.as_ref().map(|n| n.as_str()), Some("work"));
    for bad in ["", "-x", "a b", "a/b", &"n".repeat(33)] {
        assert!(args(&["--name", bad, "host"]).is_err(), "{bad:?}");
    }
    assert!(args(&["--name"]).is_err());

    let config = Config::from_text("auto_attach = true\n").unwrap();
    let mut open = args(&["host"]).unwrap();
    open.apply_config(&config);
    assert!(open.auto_attach);
    let mut off = args(&["--no-auto-attach", "host"]).unwrap();
    off.apply_config(&config);
    assert!(!off.auto_attach);
    let mut on = args(&["--auto-attach", "host"]).unwrap();
    on.apply_config(&Config::default());
    assert!(on.auto_attach);
    assert!(Config::from_text("auto_attach = \"yes\"\n").is_err());
}

#[test]
fn the_flags_of_the_listing_and_the_completion() {
    use ssj_client::cli::Command;

    let json = args(&["list", "--json", "host"]).unwrap();
    assert!(json.json && json.command == Command::List);
    assert!(!args(&["list", "host"]).unwrap().json);
    // the completion needs no destination, and takes a host or none
    assert_eq!(
        args(&["--complete"]).unwrap().command,
        Command::Complete(None)
    );
    assert_eq!(
        args(&["--complete", "me@host"]).unwrap().command,
        Command::Complete(Some("me@host".to_owned()))
    );
    assert!(args(&["--batch", "list", "host"]).unwrap().batch);
    // an option given twice: the last one counts, and the lists collect
    assert_eq!(
        args(&["--udp-host", "a", "--udp-host", "b", "h"])
            .unwrap()
            .udp_host
            .as_deref(),
        Some("b")
    );
    assert_eq!(
        args(&["--server-arg", "--x", "--server-arg", "y", "h"])
            .unwrap()
            .server_args,
        ["--x", "y"]
    );
}

#[test]
fn the_man_page_and_the_completions_are_built() {
    let found = |name: &str| std::path::Path::new(env!("SSJ_ASSETS")).join(name);
    let page = std::fs::read_to_string(found("ssj.1")).unwrap();
    for word in [
        "transport",
        "client.toml",
        "server.toml",
        "port_range",
        "auto_attach",
    ] {
        assert!(page.contains(word), "{word}");
    }
    for file in ["ssj.bash", "_ssj", "ssj.fish"] {
        let text = std::fs::read_to_string(found(file)).unwrap();
        assert!(text.contains("transport"), "{file}");
    }
}

#[test]
fn local_echo_is_off_unless_the_flag_or_the_file_turns_it_on() {
    use ssj_client::cli::{LocalEcho, parse_local_echo};
    use ssj_client::config::Config;

    assert_eq!(args(&["host"]).unwrap().local_echo, LocalEcho::Off);
    assert_eq!(
        args(&["--local-echo", "auto", "host"]).unwrap().local_echo,
        LocalEcho::Auto
    );
    assert!(args(&["--local-echo", "on", "host"]).is_err());
    assert!(parse_local_echo("").is_err());

    let config = Config::from_text("local_echo = \"auto\"\n").unwrap();
    let mut open = args(&["host"]).unwrap();
    open.apply_config(&config);
    assert_eq!(open.local_echo, LocalEcho::Auto);
    let mut off = args(&["--local-echo", "off", "host"]).unwrap();
    off.apply_config(&config);
    assert_eq!(off.local_echo, LocalEcho::Off);
    assert!(Config::from_text("local_echo = \"yes\"\n").is_err());
}
