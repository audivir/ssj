use ssj_proto::{Env, EnvError};

fn var(name: &str, value: &str) -> (String, String) {
    (name.to_owned(), value.to_owned())
}

#[test]
fn limits_are_inclusive() {
    let at_limit: Vec<_> = (0..Env::MAX_VARS)
        .map(|i| var(&format!("V{i}"), "x"))
        .collect();
    assert!(Env::new(at_limit.clone()).is_ok());

    let mut too_many = at_limit;
    too_many.push(var("EXTRA", "x"));
    assert_eq!(Env::new(too_many), Err(EnvError::TooMany));

    assert!(Env::new(vec![var(&"N".repeat(Env::MAX_NAME), "")]).is_ok());
    assert_eq!(
        Env::new(vec![var(&"N".repeat(Env::MAX_NAME + 1), "")]),
        Err(EnvError::TooLarge)
    );

    assert!(Env::new(vec![var("V", &"x".repeat(Env::MAX_VALUE))]).is_ok());
    assert_eq!(
        Env::new(vec![var("V", &"x".repeat(Env::MAX_VALUE + 1))]),
        Err(EnvError::TooLarge)
    );
}

// each variable is within its limits, only the sum is too large.
#[test]
fn total_size_is_limited() {
    let value = "x".repeat(Env::MAX_VALUE);
    let fits = Env::MAX_TOTAL / (Env::MAX_VALUE + 2);
    let vars = |n: usize| {
        (0..n)
            .map(|i| var(&format!("V{i}"), &value))
            .collect::<Vec<_>>()
    };
    assert!(Env::new(vars(fits)).is_ok());
    assert_eq!(Env::new(vars(fits + 1)), Err(EnvError::TooLarge));
}

#[test]
fn names_follow_shell_syntax() {
    for name in ["A", "_", "_1", "PATH", "lower_case9"] {
        assert!(Env::new(vec![var(name, "v")]).is_ok(), "{name:?}");
    }
    for name in ["", "1A", "A-B", "A B", "A=B", "Ä", "A\0", "A\n"] {
        assert_eq!(
            Env::new(vec![var(name, "v")]),
            Err(EnvError::InvalidName),
            "{name:?}"
        );
    }
}

#[test]
fn values_may_not_contain_nul_and_names_may_not_repeat() {
    assert_eq!(
        Env::new(vec![var("A", "x\0y")]),
        Err(EnvError::InvalidValue)
    );
    assert!(Env::new(vec![var("A", "line\nbreak and \u{e4}")]).is_ok());
    assert_eq!(
        Env::new(vec![var("A", "1"), var("A", "2")]),
        Err(EnvError::Duplicate)
    );
}

#[test]
fn terminal_and_locale_values_are_plain_names() {
    for (name, value) in [
        ("TERM", "xterm-256color"),
        ("TERM", "xterm-ghostty"),
        ("TERM", ""),
        ("LANG", "de_DE.UTF-8@euro"),
        ("LC_ALL", "C.UTF-8"),
        ("LC_TIME", "en_GB"),
        // other variables are not restricted.
        ("EDITOR", "/usr/bin/vi --wait"),
    ] {
        assert!(Env::new(vec![var(name, value)]).is_ok(), "{name}={value}");
    }
    let long = "a".repeat(Env::MAX_PLAIN_VALUE + 1);
    for (name, value) in [
        ("TERM", "../../tmp/evil"),
        ("TERM", "x/y"),
        ("TERM", "a\\b"),
        ("TERM", ".."),
        ("TERM", "x y"),
        ("TERM", "x\n"),
        ("TERM", "\u{e4}"),
        ("LANG", "/tmp/locale"),
        ("LC_ALL", "$(id)"),
        ("LC_MESSAGES", &long),
    ] {
        assert_eq!(
            Env::new(vec![var(name, value)]),
            Err(EnvError::InvalidValue),
            "{name}={value}"
        );
    }
    // the longest allowed value.
    let longest = "a".repeat(Env::MAX_PLAIN_VALUE);
    assert!(Env::new(vec![var("TERM", &longest)]).is_ok());
    // one bad variable rejects the whole set.
    assert_eq!(
        Env::new(vec![var("COLORTERM", "truecolor"), var("TERM", "a/b")]),
        Err(EnvError::InvalidValue)
    );
}
