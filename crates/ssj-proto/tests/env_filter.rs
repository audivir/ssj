use std::time::Duration;

use ssj_proto::{Env, EnvFilter, PatternError};

fn filter(patterns: &[&str]) -> EnvFilter {
    EnvFilter::new(patterns.iter().map(|p| p.to_string()).collect()).unwrap()
}

#[test]
fn defaults_accept_terminal_and_locale_only() {
    let filter = EnvFilter::default_patterns();
    for name in ["TERM", "LANG", "LC_ALL", "LC_CTYPE", "LC_"] {
        assert!(filter.matches(name), "{name}");
    }
    for name in ["PATH", "HOME", "TERMINFO", "LANGUAGE", "XLC_ALL", "LC"] {
        assert!(!filter.matches(name), "{name}");
    }
}

#[test]
fn wildcards_follow_shell_rules() {
    let cases = [
        ("A*", "A", true),
        ("A*", "ABC", true),
        ("*Z", "XYZ", true),
        ("*Z", "XYZ1", false),
        ("A?C", "ABC", true),
        ("A?C", "AC", false),
        ("A?C", "ABBC", false),
        ("*A*B*", "xxAyyByy", true),
        ("*A*B*", "xxByyAyy", false),
        ("**", "ANY", true),
        ("?", "", false),
    ];
    for (pattern, name, expected) in cases {
        assert_eq!(
            filter(&[pattern]).matches(name),
            expected,
            "{pattern} vs {name}"
        );
    }
}

// A matcher that backtracks over every star would take exponential time here. The match runs in
// a thread, so that a broken matcher fails the test and does not hang it.
#[test]
fn matching_is_fast_on_hostile_patterns() {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let pattern = format!("{}b", "*a".repeat(100));
        let name = "a".repeat(256);
        tx.send(filter(&[&pattern]).matches(&name)).ok();
    });
    let matched = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("matcher did not finish");
    assert!(!matched);
}

#[test]
fn wildcards_never_match_startup_variables_but_exact_names_do() {
    let startup = [
        "LD_PRELOAD",
        "LD_LIBRARY_PATH",
        "DYLD_INSERT_LIBRARIES",
        "SSH_AUTH_SOCK",
        "BASH_ENV",
        "ENV",
        "PROMPT_COMMAND",
        "IFS",
        "PS4",
    ];
    let wild = filter(&[
        "*", "L*", "?", "???", "SSH_*", "LD_*", "DYLD_*", "BASH_??V", "P*",
    ]);
    for name in startup {
        assert!(!wild.matches(name), "{name}");
    }
    assert!(wild.matches("PATH"));
    for name in startup {
        assert!(filter(&[name]).matches(name), "{name}");
    }
}

#[test]
fn apply_returns_accepted_variables_and_names_of_dropped_ones() {
    let vars = Env::new(vec![
        ("TERM".into(), "xterm".into()),
        ("LD_PRELOAD".into(), "/tmp/x.so".into()),
        ("LC_ALL".into(), "C".into()),
        ("SECRET".into(), "s".into()),
    ])
    .unwrap();

    let (accepted, dropped) = EnvFilter::default_patterns().apply(&vars);

    assert_eq!(
        accepted.iter().collect::<Vec<_>>(),
        [("TERM", "xterm"), ("LC_ALL", "C")]
    );
    assert_eq!(dropped, ["LD_PRELOAD", "SECRET"]);
}

#[test]
fn patterns_are_validated() {
    let new = |patterns: Vec<String>| EnvFilter::new(patterns);
    assert_eq!(new(vec!["".into()]), Err(PatternError::Empty));
    assert_eq!(new(vec!["A-B".into()]), Err(PatternError::InvalidCharacter));
    assert_eq!(
        new(vec!["A[0]".into()]),
        Err(PatternError::InvalidCharacter)
    );
    assert_eq!(new(vec!["A B".into()]), Err(PatternError::InvalidCharacter));
    assert_eq!(
        new(vec!["A".repeat(EnvFilter::MAX_PATTERN_LEN + 1)]),
        Err(PatternError::TooLong)
    );
    assert_eq!(
        new(vec!["A".into(); EnvFilter::MAX_PATTERNS + 1]),
        Err(PatternError::TooMany)
    );
    assert!(
        new(vec![
            "A".repeat(EnvFilter::MAX_PATTERN_LEN);
            EnvFilter::MAX_PATTERNS
        ])
        .is_ok()
    );
}
