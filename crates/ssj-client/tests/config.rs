use ssj_client::cli::parse;
use ssj_client::config::{AutoInstall, Config};

fn args(words: &[&str]) -> ssj_client::cli::Args {
    parse(words.iter().map(|w| w.to_string())).unwrap()
}

#[test]
fn defaults_apply_without_a_file_or_with_an_empty_one() {
    assert_eq!(Config::from_text("").unwrap(), Config::default());
    assert_eq!(
        Config::from_text("# only a comment\n\n   \n").unwrap(),
        Config::default()
    );
    let defaults = Config::default();
    assert_eq!(defaults.auto_install, AutoInstall::Ask);
    assert_eq!(defaults.escape, Some(b'~'));
    assert_eq!(defaults.ssh_command, ["ssh"]);
    // the variables come from the SendEnv of ssh; the file only adds.
    assert!(defaults.send_env.patterns().is_empty());
}

#[test]
fn every_key_is_read() {
    let config = Config::from_text(
        "auto_install = \"never\"  # no uploads\n\
         escape_char = \"#\"\n\
         ssh_command = \"ssh -J jump -o ForwardAgent=no\"\n\
         send_env = [\"TERM\", \"EDITOR\", \"MY_*\"]\n",
    )
    .unwrap();
    assert_eq!(config.auto_install, AutoInstall::Never);
    assert_eq!(config.escape, Some(b'#'));
    assert_eq!(
        config.ssh_command,
        ["ssh", "-J", "jump", "-o", "ForwardAgent=no"]
    );
    assert!(config.send_env.matches("EDITOR") && config.send_env.matches("MY_VAR"));
    assert!(!config.send_env.matches("LANG"), "nothing else is in it");
    assert_eq!(
        Config::from_text("escape_char = \"none\"").unwrap().escape,
        None
    );
    assert!(
        Config::from_text("send_env = []")
            .unwrap()
            .send_env
            .patterns()
            .is_empty()
    );
}

#[test]
fn mistakes_are_errors_that_name_the_problem() {
    for (text, expected) in [
        ("auto_instal = \"never\"", "line 1: unknown key"),
        ("\n\nauto_install = never", "line 3:"),
        (
            "auto_install = true",
            "line 1: auto_install must be a string",
        ),
        (
            "auto_install = \"sometimes\"",
            "auto_install must be \"ask\"",
        ),
        (
            "auto_install = \"never\"\nauto_install = \"always\"",
            "line 2: duplicate key",
        ),
        (
            "escape_char = \"ab\"",
            "escape_char: expected one printable",
        ),
        ("escape_char = \"\"", "escape_char: expected one printable"),
        (
            "escape_char = \"\\t\"",
            "escape_char: expected one printable",
        ),
        ("ssh_command = \"   \"", "ssh_command is empty"),
        (
            "ssh_command = [\"ssh\"]",
            "line 1: ssh_command must be a string",
        ),
        ("send_env = \"TERM\"", "line 1: send_env must be a list"),
        ("send_env = [\"A-B\"]", "send_env: InvalidCharacter"),
        ("[section]", "line 1:"),
    ] {
        let error = Config::from_text(text).unwrap_err();
        assert!(error.contains(expected), "{text:?}: {error}");
    }
}

#[test]
fn the_command_line_wins_over_the_file_and_the_file_over_the_defaults() {
    let config = Config::from_text(
        "auto_install = \"always\"\nescape_char = \"#\"\nssh_command = \"my-ssh\"\nsend_env = [\"ONLY\"]\n",
    )
    .unwrap();

    // nothing on the command line: the file decides.
    let mut plain = args(&["host"]);
    plain.apply_config(&config);
    assert_eq!(plain.ssh_command, ["my-ssh"]);
    assert_eq!(plain.escape, Some(b'#'));
    assert!(plain.send_env.matches("ONLY") && !plain.send_env.matches("LANG"));
    assert_eq!(plain.auto_install, Some(AutoInstall::Always));

    // each flag beats its key, and leaves the other keys alone.
    let mut flags = args(&[
        "--ssh-command",
        "other-ssh",
        "--escape-char",
        "none",
        "--send-env",
        "X",
        "--no-install",
        "host",
    ]);
    flags.apply_config(&config);
    assert_eq!(flags.ssh_command, ["other-ssh"]);
    assert_eq!(flags.escape, None);
    // --send-env comes on top of the file and does not replace it.
    assert!(flags.send_env.matches("ONLY") && !flags.send_env.matches("X"));
    assert_eq!(flags.send_env_add, ["X"]);
    assert_eq!(flags.auto_install, Some(AutoInstall::Never));

    let mut one = args(&["--escape-char", "!", "host"]);
    one.apply_config(&config);
    assert_eq!(
        (one.escape, one.ssh_command),
        (Some(b'!'), vec!["my-ssh".to_owned()])
    );
}

#[test]
fn the_detach_key_is_a_control_key_or_none() {
    assert_eq!(Config::default().detach_key, None);
    for (text, byte) in [
        ("ctrl-]", 0x1d),
        ("CTRL-]", 0x1d),
        ("^]", 0x1d),
        ("ctrl-a", 0x01),
        ("^Z", 0x1a),
        ("ctrl-\\\\", 0x1c),
        ("ctrl-_", 0x1f),
    ] {
        let config = Config::from_text(&format!("detach_key = \"{text}\"")).unwrap();
        assert_eq!(config.detach_key, Some(byte), "{text}");
    }
    assert_eq!(
        Config::from_text("detach_key = \"none\"")
            .unwrap()
            .detach_key,
        None
    );
    for bad in [
        "", "]", "ctrl-", "ctrl-ab", "ctrl-1", "ctrl-i", "ctrl-m", "ctrl-j", "x",
    ] {
        let error = Config::from_text(&format!("detach_key = \"{bad}\"")).unwrap_err();
        assert!(error.starts_with("detach_key: "), "{bad}: {error}");
    }
}
