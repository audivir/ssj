use ssj_proto::bootstrap::{BootstrapError, Connect, format_connect, parse_connect, sanitize};

fn connect() -> Connect {
    let mut fingerprint = [0u8; 32];
    for (i, byte) in fingerprint.iter_mut().enumerate() {
        *byte = (i * 8 + 3) as u8;
    }
    Connect {
        port: 60123,
        fingerprint,
    }
}

const HEX: &str = "030b131b232b333b434b535b636b737b838b939ba3abb3bbc3cbd3dbe3ebf3fb";

#[test]
fn line_has_a_fixed_shape() {
    assert_eq!(
        format_connect(&connect()),
        format!("SSJ CONNECT 60123 {HEX}\n")
    );
}

#[test]
fn finds_the_line_between_banners_and_noise() {
    let line = format_connect(&connect());
    let output = format!("Welcome to host\r\nlast login: today\n{line}some trailing text\n");
    assert_eq!(parse_connect(output.as_bytes()), Ok(Some(connect())));
    // windows line ending, as some shells and wrappers produce
    let crlf = line.replace('\n', "\r\n");
    assert_eq!(parse_connect(crlf.as_bytes()), Ok(Some(connect())));
}

#[test]
fn waits_for_the_complete_line() {
    let line = format_connect(&connect());
    for cut in 0..line.len() - 1 {
        assert_eq!(
            parse_connect(&line.as_bytes()[..cut]),
            Ok(None),
            "cut at {cut}"
        );
    }
    assert_eq!(parse_connect(b""), Ok(None));
    assert_eq!(parse_connect(b"just a banner\n"), Ok(None));
}

// Text that merely contains the prefix, such as an echoed command, is not the line.
#[test]
fn prefix_must_be_at_the_start_of_the_line() {
    for text in [
        format!(" SSJ CONNECT 1 {HEX}\n"),
        format!("echo SSJ CONNECT 1 {HEX}\n"),
        format!("ssj CONNECT 1 {HEX}\n"),
        format!("SSJ  CONNECT 1 {HEX}\n"),
    ] {
        assert_eq!(parse_connect(text.as_bytes()), Ok(None), "{text:?}");
    }
}

#[test]
fn malformed_lines_are_errors_and_not_ignored() {
    let upper = HEX.to_uppercase();
    let cases = [
        format!("SSJ CONNECT 0 {HEX}\n"),
        format!("SSJ CONNECT 65536 {HEX}\n"),
        format!("SSJ CONNECT 080 {HEX}\n"),
        format!("SSJ CONNECT +80 {HEX}\n"),
        format!("SSJ CONNECT -1 {HEX}\n"),
        format!("SSJ CONNECT 123456 {HEX}\n"),
        format!("SSJ CONNECT  {HEX}\n"),
        format!("SSJ CONNECT 80 {upper}\n"),
        format!("SSJ CONNECT 80 {}\n", &HEX[1..]),
        format!("SSJ CONNECT 80 {HEX}00\n"),
        format!("SSJ CONNECT 80 {}g\n", &HEX[..63]),
        format!("SSJ CONNECT 80\t{HEX}\n"),
        format!("SSJ CONNECT 80 {HEX} extra\n"),
        format!("SSJ CONNECT 80 {HEX} \n"),
        "SSJ CONNECT \n".to_owned(),
    ];
    for text in cases {
        assert_eq!(
            parse_connect(text.as_bytes()),
            Err(BootstrapError::Malformed),
            "{text:?}"
        );
    }
}

#[test]
fn two_lines_are_ambiguous_even_if_identical() {
    let line = format_connect(&connect());
    assert_eq!(
        parse_connect(format!("{line}{line}").as_bytes()),
        Err(BootstrapError::Ambiguous)
    );
}

#[test]
fn remote_text_cannot_carry_terminal_commands() {
    // clear screen, set title, and a fake prompt, as a hostile banner could send
    let hostile = b"\x1b[2J\x1b]0;title\x07\x1b[31mred\r\x9b31m";
    let shown = sanitize(hostile, 100);
    assert!(!shown.chars().any(|c| c.is_control()), "{shown:?}");
    assert!(shown.contains("red"));

    // right-to-left override can reorder what the user reads
    assert_eq!(sanitize("a\u{202e}b\u{2066}c".as_bytes(), 100), "a?b?c");
    assert_eq!(sanitize(b"\xff\xfe", 100), "\u{fffd}\u{fffd}");
}

#[test]
fn sanitized_text_is_cut_to_the_limit() {
    assert_eq!(sanitize(b"abcdef", 6), "abcdef");
    assert_eq!(sanitize(b"abcdefg", 6), "abcdef...");
    assert_eq!(sanitize("äöüäöüx".as_bytes(), 6), "äöüäöü...");
}
