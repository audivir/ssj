use ssj_client::config::AutoInstall;
use ssj_client::install::{Decision, Target, decide, parse_target, sha256_hex};

#[test]
fn systems_with_a_server_build_are_recognised_exactly() {
    for (line, target, triple) in [
        (
            "SSJ TARGET Linux x86_64",
            Target::LinuxX86_64,
            "x86_64-unknown-linux-musl",
        ),
        (
            "SSJ TARGET Linux aarch64",
            Target::LinuxAarch64,
            "aarch64-unknown-linux-musl",
        ),
        (
            "SSJ TARGET Darwin arm64",
            Target::MacArm64,
            "aarch64-apple-darwin",
        ),
    ] {
        let parsed = parse_target(format!("{line}\n").as_bytes()).unwrap();
        assert_eq!((parsed, parsed.triple()), (target, triple));
    }
}

#[test]
fn everything_else_is_refused_with_a_readable_message() {
    for output in [
        "SSJ TARGET Darwin x86_64\n",
        "SSJ TARGET Linux armv7l\n",
        "SSJ TARGET FreeBSD amd64\n",
        "SSJ TARGET Linux x86_64 extra\n",
        "SSJ TARGET linux x86_64\n",
        "SSJ TARGET \n",
        "Linux x86_64\n",
        "",
    ] {
        assert!(parse_target(output.as_bytes()).is_err(), "{output:?}");
    }
    // two answers could be an attempt to mislead.
    assert!(parse_target(b"SSJ TARGET Linux x86_64\nSSJ TARGET Darwin arm64\n").is_err());
    // noise from a login script around the answer is fine.
    assert_eq!(
        parse_target(b"welcome!\nSSJ TARGET Linux x86_64\nbye\n"),
        Ok(Target::LinuxX86_64)
    );
    // text of the host is not shown raw.
    let error = parse_target(b"SSJ TARGET \x1b[2JLinux\n").unwrap_err();
    assert!(!error.chars().any(char::is_control), "{error:?}");
}

#[test]
fn digest_is_lowercase_hex_of_sha256() {
    assert_eq!(
        sha256_hex(b""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(
        sha256_hex(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[test]
fn the_policy_decides_whether_and_whom_to_ask() {
    let host = "me@host";
    let never_asked = |_: &str| panic!("nobody should be asked");

    assert_eq!(
        decide(AutoInstall::Always, host, false, never_asked),
        Decision::Install
    );
    assert_eq!(
        decide(AutoInstall::Always, host, true, never_asked),
        Decision::Install
    );

    for interactive in [false, true] {
        let Decision::Skip(message) = decide(AutoInstall::Never, host, interactive, never_asked)
        else {
            panic!("never installs");
        };
        assert!(message.contains("ssj install me@host"), "{message}");
    }

    // without a terminal there is nobody to ask, and the message says what to do.
    let Decision::Skip(message) = decide(AutoInstall::Ask, host, false, never_asked) else {
        panic!("cannot ask without a terminal");
    };
    assert!(
        message.contains("no terminal") && message.contains("--install"),
        "{message}"
    );

    let mut question = String::new();
    let decision = decide(AutoInstall::Ask, host, true, |q| {
        question = q.to_owned();
        true
    });
    assert_eq!(decision, Decision::Install);
    assert!(
        question.contains("me@host") && question.ends_with("[y/N] "),
        "{question}"
    );

    assert!(matches!(
        decide(AutoInstall::Ask, host, true, |_| false),
        Decision::Skip(_)
    ));
}
