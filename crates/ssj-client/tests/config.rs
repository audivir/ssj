use ssj_client::config::{AutoInstall, Config};

#[test]
fn defaults_apply_without_a_file_or_with_an_empty_one() {
    assert_eq!(Config::from_text("").unwrap(), Config::default());
    assert_eq!(
        Config::from_text("# only a comment\n\n   \n").unwrap(),
        Config::default()
    );
    assert_eq!(Config::default().auto_install, AutoInstall::Ask);
}

#[test]
fn auto_install_takes_three_words() {
    for (text, expected) in [
        ("auto_install = \"ask\"", AutoInstall::Ask),
        ("auto_install = \"always\"", AutoInstall::Always),
        (
            "auto_install=\"never\"  # no uploads please",
            AutoInstall::Never,
        ),
        ("  auto_install   =   \"never\"\n", AutoInstall::Never),
    ] {
        assert_eq!(
            Config::from_text(text).unwrap().auto_install,
            expected,
            "{text:?}"
        );
    }
}

#[test]
fn mistakes_are_errors_with_the_line_number() {
    for (text, line) in [
        ("auto_instal = \"never\"", 1),
        ("\n\nauto_install = never", 3),
        ("auto_install = true", 1),
        ("auto_install = 1", 1),
        ("auto_install = \"sometimes\"", 1),
        ("auto_install = \"never\"\nauto_install = \"always\"", 2),
        ("[section]", 1),
        ("auto_install", 1),
        ("auto_install = \"never", 1),
        ("auto_install = \"ne\\ver\"", 1),
        ("Auto_Install = \"never\"", 1),
        ("= \"never\"", 1),
    ] {
        let error = Config::from_text(text).unwrap_err();
        assert!(
            error.starts_with(&format!("line {line}:")),
            "{text:?}: {error}"
        );
    }
}

// A comment sign inside a string is part of the string.
#[test]
fn hash_inside_a_string_is_not_a_comment() {
    let error = Config::from_text("auto_install = \"a # b\"").unwrap_err();
    assert!(error.contains("ask, always, or never"), "{error}");
}
