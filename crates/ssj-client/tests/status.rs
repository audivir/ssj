use ssj_client::status::{Status, TitleStatus};

#[test]
fn the_old_title_is_restored_by_the_stack_and_by_name() {
    let mut status = TitleStatus::default();
    let shown = status.show("no response from the server");
    assert_eq!(shown, b"\x1b[22;0t\x1b]2;no response from the server\x07");
    // a second message does not push the stack again
    assert_eq!(status.show("again"), b"\x1b]2;again\x07");

    assert_eq!(
        status.clear(Some("my title")),
        b"\x1b[23;0t\x1b]2;my title\x07"
    );
    // nothing to clear any more
    assert!(status.clear(Some("my title")).is_empty());
}

#[test]
fn without_a_known_title_only_the_stack_is_used() {
    let mut status = TitleStatus::default();
    status.show("x");
    assert_eq!(status.clear(None), b"\x1b[23;0t");
}

#[test]
fn control_characters_cannot_end_the_title_early() {
    let mut status = TitleStatus::default();
    let shown = status.show("a\x07b\x1bc");
    assert_eq!(shown, b"\x1b[22;0t\x1b]2;abc\x07");
}
