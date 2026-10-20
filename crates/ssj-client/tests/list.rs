//! The lines of `ssj list`.

use ssj_client::run::list_lines;
use ssj_proto::control::{SessionInfo, SessionState};
use ssj_proto::{SessionId, SessionName};

fn info(id: &str, name: Option<&str>, state: SessionState, age_secs: u64) -> SessionInfo {
    SessionInfo {
        id: SessionId::parse(id).unwrap(),
        name: name.map(|n| SessionName::parse(n).unwrap()),
        state,
        age_secs,
        protocol: 1,
    }
}

#[test]
fn a_header_names_the_columns_and_the_rows_line_up_under_it() {
    let infos = [
        info("k5l6w3qagivlee3m", Some("test"), SessionState::Detached, 12),
        info("abcdef012345", None, SessionState::Connected, 4000),
    ];
    let lines = list_lines(&infos, true);
    assert_eq!(lines.len(), 3);
    assert!(lines[0].starts_with("ID  "), "{:?}", lines[0]);
    assert!(
        lines[0].contains("STATE") && lines[0].contains("AGE") && lines[0].ends_with("NAME"),
        "{:?}",
        lines[0]
    );
    // every column starts at the same place in the header and in the rows
    for word in ["STATE", "AGE", "PROTOCOL", "NAME"] {
        let column = lines[0].find(word).unwrap();
        let row = &lines[1];
        assert!(
            column < row.len() && row[..column].ends_with(' '),
            "{word} at {column}: {row:?}"
        );
    }
    assert!(
        lines[1].starts_with("k5l6w3qagivlee3m  detached"),
        "{:?}",
        lines[1]
    );
    assert!(lines[1].ends_with("protocol 1  test"), "{:?}", lines[1]);
    assert!(lines[2].ends_with("protocol 1  -"), "{:?}", lines[2]);
}

#[test]
fn without_a_header_the_rows_are_the_same_and_an_empty_list_has_none() {
    let infos = [info(
        "abcdef012345",
        Some("work"),
        SessionState::Connected,
        30,
    )];
    let with = list_lines(&infos, true);
    let without = list_lines(&infos, false);
    assert_eq!(without.len(), 1);
    assert_eq!(with[1], without[0]);
    assert!(list_lines(&[], true).is_empty());
}
