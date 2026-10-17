use ssj_config::parse;

#[test]
fn empty_files_and_comments_are_fine() {
    for text in [
        "",
        "\n\n  \n",
        "# only a comment\n",
        "   # indented comment\n",
    ] {
        parse(text).unwrap().finish().unwrap();
    }
}

#[test]
fn values_are_read_by_their_type() {
    let mut table = parse(
        "name = \"a # b\"  # comment\n\
         items = [\"x\", \"y z\" ,\"q\\\"r\", ]\n\
         none = []\n\
         limit = 90\n\
         off = \"none\"\n",
    )
    .unwrap();
    assert_eq!(table.take_text("name").unwrap(), Some("a # b".to_owned()));
    assert_eq!(
        table.take_list("items").unwrap(),
        Some(vec!["x".into(), "y z".into(), "q\"r".into()])
    );
    assert_eq!(table.take_list("none").unwrap(), Some(vec![]));
    assert_eq!(table.take_seconds("limit").unwrap(), Some(Some(90)));
    assert_eq!(table.take_seconds("off").unwrap(), Some(None));
    assert_eq!(table.take_text("absent").unwrap(), None);
    table.finish().unwrap();
}

#[test]
fn syntax_errors_name_their_line() {
    for (text, line) in [
        ("a = ", 1),
        ("\n\na = b", 3),
        ("a", 1),
        ("= \"x\"", 1),
        ("A = \"x\"", 1),
        ("a-b = \"x\"", 1),
        ("a = \"x\"\na = \"y\"", 2),
        ("a = \"unterminated", 1),
        ("a = \"x\" y", 1),
        ("a = \"ne\\ver\"", 1),
        ("a = [\"x\" \"y\"]", 1),
        ("a = [\"x\"", 1),
        ("a = [x]", 1),
        ("a = [\"x\"] y", 1),
        ("a = [[\"x\"]]", 1),
        ("[section]", 1),
        ("a = 1.5", 1),
    ] {
        let error = parse(text)
            .err()
            .unwrap_or_else(|| panic!("{text:?} was accepted"));
        assert!(
            error.starts_with(&format!("line {line}:")),
            "{text:?}: {error}"
        );
    }
}

#[test]
fn wrong_types_and_leftover_keys_are_errors_with_the_line() {
    let table = || parse("\ntext = \"t\"\nitems = [\"a\"]\nnumber = 5\nneg = -1\n").unwrap();
    assert!(
        table()
            .take_list("text")
            .unwrap_err()
            .starts_with("line 2:")
    );
    assert!(
        table()
            .take_text("items")
            .unwrap_err()
            .starts_with("line 3:")
    );
    assert!(
        table()
            .take_text("number")
            .unwrap_err()
            .starts_with("line 4:")
    );
    assert!(
        table()
            .take_seconds("text")
            .unwrap_err()
            .starts_with("line 2:")
    );
    assert!(
        table()
            .take_seconds("neg")
            .unwrap_err()
            .contains("negative")
    );

    let mut left = table();
    left.take_text("text").unwrap();
    let error = left.finish().unwrap_err();
    assert!(
        error.starts_with("line 3:") && error.contains("items"),
        "{error}"
    );
}

// A comment sign inside a string, even after an escaped quote, belongs to the string.
#[test]
fn comment_detection_follows_the_strings() {
    let mut table = parse("a = \"x \\\" # still inside\" # real comment\n").unwrap();
    assert_eq!(
        table.take_text("a").unwrap(),
        Some("x \" # still inside".to_owned())
    );
}
