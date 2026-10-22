use std::process::Command;

use ssj_client::bootstrap::{remote_command, shell_quote};
use ssj_client::cli::{Args, ServerPath, misplaced_option, parse};

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
        vec!["--bogus", "host"],
        vec!["--ssh-command"],
        vec!["--ssh-command", "  ", "host"],
        vec!["host with space"],
        vec!["--send-env", "A-B", "host"],
    ] {
        assert!(args(&bad).is_err(), "{bad:?}");
    }
}

// the destination is passed after `--`, but an option-like word must not get that far at all.
#[test]
fn destination_that_looks_like_an_option_is_refused() {
    assert!(args(&["-oProxyCommand=touch /tmp/x"]).is_err());
}

// whatever the user types for the server path or its arguments must reach the server
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

// the default command is run by the remote login shell like this, so the test uses a real shell.
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

    // nothing installed: a marker and a code that the client can tell apart from other failures.
    let missing = run(&parsed);
    assert_eq!(missing.status.code(), Some(66));
    assert_eq!(String::from_utf8_lossy(&missing.stdout), "SSJ MISSING\n");

    // installed: the file runs, and every argument arrives as one word.
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

    // a file that is not executable counts as missing.
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(run(&parsed).status.code(), Some(66));

    // a path given by the user is used as it is, without the wrapper.
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
            vec!["attach", "host", "abcdef012345"],
            Command::Attach(Some(id.clone())),
            "host",
        ),
        (vec!["kill", "host"], Command::Kill(Vec::new()), "host"),
        (
            vec!["kill", "host", "abcdef012345"],
            Command::Kill(vec![id]),
            "host",
        ),
        // a name works where an id does.
        (
            vec!["attach", "host", "work"],
            Command::Attach(Some("work".to_owned())),
            "host",
        ),
        (
            vec!["kill", "host", "My-Session"],
            Command::Kill(vec!["My-Session".to_owned()]),
            "host",
        ),
        (
            vec!["kill", "host", "one", "two", "three"],
            Command::Kill(vec!["one".to_owned(), "two".to_owned(), "three".to_owned()]),
            "host",
        ),
        (vec!["detach", "host"], Command::Detach(Vec::new()), "host"),
        (
            vec!["detach", "host", "work", "other"],
            Command::Detach(vec!["work".to_owned(), "other".to_owned()]),
            "host",
        ),
        // the options that the command reads may stand before it too
        (vec!["-v", "list", "host", "--json"], Command::List, "host"),
        (
            vec!["--ssh-command", "ssh -J x", "list", "host"],
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
        vec!["kill", "host", "../../x"],
        vec!["kill", "host", "a b"],
        vec!["attach", "a", "b", "id"],
    ] {
        assert!(args(&bad).is_err(), "{bad:?}");
    }
}

#[test]
fn a_command_refuses_the_options_of_the_connection() {
    // the setting belongs in client.toml, not before a command that does not read it
    let message = args(&["--send-env=LC_THEME", "list", "host"]).unwrap_err();
    assert!(
        message.contains("--send-env is not an option of `ssj list`")
            && message.contains("client.toml"),
        "{message}"
    );
    assert!(args(&["--detach-key", "none", "list", "host"]).is_err());
    assert!(args(&["kill", "host", "--json"]).is_err());
    assert!(args(&["list", "host", "--force"]).is_err());
    // the connection reads them
    assert!(args(&["--send-env=LC_THEME", "host"]).is_ok());
    assert!(args(&["--detach-key", "none", "attach", "host"]).is_ok());
    assert!(args(&["--force", "kill", "host"]).is_ok());
}

#[test]
fn connect_is_the_plain_connection_to_a_host_with_the_name_of_a_command() {
    use ssj_client::cli::Command;

    let parsed = args(&["connect", "list"]).unwrap();
    assert_eq!(parsed.command, Command::Connect);
    assert_eq!(parsed.destination, "list");
    let parsed = args(&["connect", "host", "ls", "-l"]).unwrap();
    assert_eq!(parsed.command, Command::Connect);
    assert_eq!(parsed.destination, "host");
    assert_eq!(parsed.remote_command.as_deref(), Some("ls -l"));
    // the options of the connection, before the destination
    let parsed = args(&["connect", "--send-env=LC_THEME", "host"]).unwrap();
    assert_eq!(parsed.command, Command::Connect);
    // `ssj edit` alone is the editor, and `ssj connect edit` is the host
    assert_eq!(args(&["edit"]).unwrap().command, Command::Edit);
    let parsed = args(&["connect", "edit"]).unwrap();
    assert_eq!(parsed.command, Command::Connect);
    assert_eq!(parsed.destination, "edit");
    assert_eq!(args(&["edit", "edit"]).unwrap().destination, "edit");
    // a destination is needed
    assert!(
        args(&["connect"])
            .unwrap_err()
            .contains("missing destination")
    );
}

#[test]
fn a_lone_command_word_is_a_host_with_a_warning_that_names_the_way_out() {
    use ssj_client::cli::lone_command_hint;
    for word in ["list", "attach", "kill", "detach", "install"] {
        let hint = lone_command_hint(word).unwrap();
        assert!(hint.contains(&format!("ssj connect {word}")), "{hint}");
        // the word alone is still the host
        assert_eq!(args(&[word]).unwrap().destination, word);
    }
    // `ssj edit` alone is the editor, and other words are hosts
    assert!(lone_command_hint("edit").is_none());
    assert!(lone_command_hint("host").is_none());
    assert!(lone_command_hint("connect").is_none());
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
    use ssj_client::cli::Transport;
    use ssj_client::config::Config;

    assert_eq!(args(&["host"]).unwrap().transport, Transport::Auto);
    for (word, transport) in [
        ("auto", Transport::Auto),
        ("udp", Transport::Udp),
        ("tcp", Transport::Tcp),
        ("stdio", Transport::Stdio),
    ] {
        let parsed = args(&["--transport", word, "host"]).unwrap();
        assert_eq!(parsed.transport, transport);
        assert!(parsed.explicit.transport);
    }
    for bad in ["", "UDP", "quic", "ssh"] {
        assert!(args(&["--transport", bad, "host"]).is_err(), "{bad:?}");
    }
    assert!(args(&["--transport"]).is_err());
    assert!(args(&["--transport", "x", "host"]).is_err());

    // the file fills in what the command line leaves open.
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
    // the host part is what DNS gets.
    assert_eq!(args(&["tim@host:2222"]).unwrap().host(), "host");
    assert_eq!(args(&["[::1]:22"]).unwrap().host(), "::1");
    // an option or a command in place of a destination.
    for bad in ["-oProxyCommand=id", "host;id", "a b", "$(id)", "host:0", ""] {
        assert!(args(&[bad]).is_err(), "{bad:?}");
    }
    // an option is no destination even after the other arguments.
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
fn congestion_is_a_flag_and_a_file_key() {
    use ssj_client::config::Config;
    use ssj_net::Congestion;

    assert_eq!(args(&["host"]).unwrap().congestion, None);
    assert_eq!(
        args(&["--congestion", "cubic", "host"]).unwrap().congestion,
        Some(Congestion::Cubic)
    );
    assert!(args(&["--congestion", "reno", "host"]).is_err());
    let config = Config::from_text("congestion = \"cubic\"\n").unwrap();
    let mut from_file = args(&["host"]).unwrap();
    from_file.apply_config(&config);
    assert_eq!(from_file.congestion, Some(Congestion::Cubic));
    let mut flag_wins = args(&["--congestion", "bbr", "host"]).unwrap();
    flag_wins.apply_config(&config);
    assert_eq!(flag_wins.congestion, Some(Congestion::Bbr));
    assert!(Config::from_text("congestion = \"reno\"\n").is_err());
}

#[test]
fn agent_forwarding_is_off_unless_the_flag_or_the_file_turns_it_on() {
    use ssj_client::config::Config;

    assert!(!args(&["host"]).unwrap().forward_agent);
    assert!(args(&["-A", "host"]).unwrap().forward_agent);
    assert!(args(&["--forward-agent", "host"]).unwrap().forward_agent);
    let config = Config::from_text("forward_agent = true\n").unwrap();
    let mut from_file = args(&["host"]).unwrap();
    from_file.apply_config(&config);
    assert!(from_file.forward_agent);
    let mut off = args(&["--no-forward-agent", "host"]).unwrap();
    off.apply_config(&config);
    assert!(!off.forward_agent);
    assert!(Config::from_text("forward_agent = \"yes\"\n").is_err());
}

#[test]
fn the_flags_of_the_listing_and_the_completion() {
    use ssj_client::cli::Command;

    let json = args(&["list", "--json", "host"]).unwrap();
    assert!(json.json && json.command == Command::List);
    assert!(!args(&["list", "host"]).unwrap().json);
    // the completion needs no destination, and takes a host or none.
    assert_eq!(
        args(&["--complete"]).unwrap().command,
        Command::Complete {
            host: None,
            connected: false
        }
    );
    assert_eq!(
        args(&["--complete", "me@host"]).unwrap().command,
        Command::Complete {
            host: Some("me@host".to_owned()),
            connected: false
        }
    );
    assert_eq!(
        args(&["--complete", "me@host", "--connected"])
            .unwrap()
            .command,
        Command::Complete {
            host: Some("me@host".to_owned()),
            connected: true
        }
    );
    assert!(args(&["--batch", "list", "host"]).unwrap().batch);
    // an option given twice: the last one counts, and the lists collect.
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
fn local_echo_is_off_unless_the_flag_or_the_file_turns_it_on() {
    use ssj_client::cli::LocalEcho;
    use ssj_client::config::Config;

    assert_eq!(args(&["host"]).unwrap().local_echo, LocalEcho::Off);
    assert_eq!(
        args(&["--local-echo", "auto", "host"]).unwrap().local_echo,
        LocalEcho::Auto
    );
    assert!(args(&["--local-echo", "on", "host"]).is_err());
    assert!(args(&["--local-echo", "", "host"]).is_err());

    let config = Config::from_text("local_echo = \"auto\"\n").unwrap();
    let mut open = args(&["host"]).unwrap();
    open.apply_config(&config);
    assert_eq!(open.local_echo, LocalEcho::Auto);
    let mut off = args(&["--local-echo", "off", "host"]).unwrap();
    off.apply_config(&config);
    assert_eq!(off.local_echo, LocalEcho::Off);
    assert!(Config::from_text("local_echo = \"yes\"\n").is_err());
}

#[test]
fn the_help_shows_a_flag_and_its_counterpart_on_one_line() {
    let help = ssj_client::options::command()
        .render_long_help()
        .to_string();
    for line in [
        "--auto-attach / --no-auto-attach",
        "-A, --forward-agent / --no-forward-agent",
        "--install / --no-install",
    ] {
        assert!(help.contains(line), "{line}\n{help}");
    }
    // the counterparts have no line of their own.
    assert!(!help.contains("\n      --no-auto-attach"));
    // and both spellings still work.
    assert!(args(&["--auto-attach", "h"]).unwrap().auto_attach);
    assert!(
        !args(&["--auto-attach", "--no-auto-attach", "h"])
            .unwrap()
            .auto_attach
    );
    assert!(args(&["--install", "h"]).is_ok() && args(&["--no-install", "h"]).is_ok());
}

#[test]
fn verbose_counts_up_to_three_and_the_log_file_is_a_path() {
    assert_eq!(args(&["h"]).unwrap().verbose, 0);
    assert_eq!(args(&["-v", "h"]).unwrap().verbose, 1);
    assert_eq!(args(&["-vv", "h"]).unwrap().verbose, 2);
    assert_eq!(args(&["-v", "-v", "-v", "h"]).unwrap().verbose, 3);
    assert_eq!(args(&["-vvvvv", "h"]).unwrap().verbose, 3);
    assert_eq!(args(&["--verbose", "h"]).unwrap().verbose, 1);
    let with_file = args(&["--log-file", "/tmp/ssj.log", "h"]).unwrap();
    assert_eq!(
        with_file.log_file.as_deref(),
        Some(std::path::Path::new("/tmp/ssj.log"))
    );
    assert!(args(&["--log-file"]).is_err());
}

#[test]
fn edit_alone_is_the_local_editor_and_with_a_host_the_remote_one() {
    use ssj_client::cli::Command;
    let local = args(&["edit"]).unwrap();
    assert_eq!(
        (local.command, local.destination.as_str()),
        (Command::Edit, "")
    );
    let remote = args(&["edit", "me@host:2222"]).unwrap();
    assert_eq!(remote.command, Command::Edit);
    assert_eq!(
        (remote.destination.as_str(), remote.ssh_port),
        ("me@host", Some(2222))
    );
    // a host called edit is reached with a user in front.
    let host = args(&["me@edit"]).unwrap();
    assert_eq!(
        (host.command, host.destination.as_str()),
        (Command::Connect, "me@edit")
    );
    assert!(args(&["edit", "a", "b"]).is_err());
}

#[test]
fn a_command_after_the_destination_runs_in_the_session_as_with_ssh() {
    use ssj_client::cli::Command;
    let cases: Vec<(Vec<&str>, Command, &str, Option<&str>)> = vec![
        (vec!["host"], Command::Connect, "host", None),
        (vec!["host", "ls"], Command::Connect, "host", Some("ls")),
        (
            vec!["host", "echo", "hi", "there"],
            Command::Connect,
            "host",
            Some("echo hi there"),
        ),
        // as with `ssh`, everything after the destination is the command, options included.
        (
            vec!["host", "ls", "-l"],
            Command::Connect,
            "host",
            Some("ls -l"),
        ),
        (
            vec!["--local-echo", "auto", "host", "make", "-j4"],
            Command::Connect,
            "host",
            Some("make -j4"),
        ),
        (
            vec!["host", "--name", "x"],
            Command::Connect,
            "host",
            Some("--name x"),
        ),
        (
            vec!["a", "b", "--", "ls"],
            Command::Connect,
            "a",
            Some("b -- ls"),
        ),
        // the commands of ssj keep their options after the destination.
        (vec!["list", "host", "--json"], Command::List, "host", None),
        // the `--` is optional, and options of ssj stay in front.
        (
            vec!["host", "--", "ls", "-l"],
            Command::Connect,
            "host",
            Some("ls -l"),
        ),
        (
            vec!["--local-echo", "auto", "host", "--", "make", "-j4"],
            Command::Connect,
            "host",
            Some("make -j4"),
        ),
        (
            vec!["me@host:2222", "--", "uptime"],
            Command::Connect,
            "me@host",
            Some("uptime"),
        ),
        // a shell command in one word is passed as it is.
        (
            vec!["host", "--", "cd /tmp && ls | wc -l"],
            Command::Connect,
            "host",
            Some("cd /tmp && ls | wc -l"),
        ),
        // hosts that are called like our commands: after `--` the single word is the host.
        (
            vec!["attach", "--", "ls"],
            Command::Connect,
            "attach",
            Some("ls"),
        ),
        (
            vec!["list", "--", "ls"],
            Command::Connect,
            "list",
            Some("ls"),
        ),
        // a `--` before the destination ends the options, as with `ssh`.
        (vec!["--", "ls"], Command::Connect, "ls", None),
        // and without it the command word wins, as always.
        (vec!["attach", "ls"], Command::Attach(None), "ls", None),
        (vec!["list", "ls"], Command::List, "ls", None),
    ];
    for (words, command, destination, remote) in cases {
        let parsed = args(&words).unwrap_or_else(|e| panic!("{words:?}: {e}"));
        assert_eq!(parsed.command, command, "{words:?}");
        assert_eq!(parsed.destination, destination, "{words:?}");
        assert_eq!(parsed.remote_command.as_deref(), remote, "{words:?}");
    }
    for bad in [
        // a command of ssj runs nothing on the host.
        vec!["list", "host", "--", "ls"],
        // an empty command
        vec!["host", "--", ""],
        vec!["host", "--", " "],
        // an existing session would not run it (the options come before the destination).
        vec!["--name", "work", "host", "--", "ls"],
        vec!["--auto-attach", "host", "ls"],
    ] {
        assert!(args(&bad).is_err(), "{bad:?}");
    }
}

#[test]
fn an_option_after_the_destination_gets_a_hint_and_a_command_does_not() {
    for word in [
        "--install",
        "--install=x",
        "--transport",
        "-q",
        "-A",
        "--no-install",
    ] {
        assert!(misplaced_option(word).is_some(), "{word}");
    }
    for word in ["ls", "--nothing", "-Z", "-", "--", "echo"] {
        assert!(misplaced_option(word).is_none(), "{word}");
    }
    // it is still run as a command, as `ssh` would.
    let parsed = args(&["host", "--install"]).unwrap();
    assert_eq!(parsed.remote_command.as_deref(), Some("--install"));
}

#[test]
fn send_env_is_kept_for_the_session_to_add_to_the_patterns_of_ssh() {
    let parsed = args(&["--send-env", "LC_THEME", "--send-env", "EDITOR", "host"]).unwrap();
    assert_eq!(parsed.send_env_add, ["LC_THEME", "EDITOR"]);
    assert!(parsed.send_env.patterns().is_empty());
    assert!(args(&["--send-env", "A-B", "host"]).is_err());
}
