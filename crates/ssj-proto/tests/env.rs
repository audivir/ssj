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

// Each variable is within its limits, only the sum is too large.
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
