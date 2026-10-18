//! A descriptor that a parent leaves open must not reach the server or its shell.

use std::process::Command;

use ssj_server::daemon::close_inherited_descriptors;

const CHILD: &str = "SSJ_TEST_FD_CHILD";

fn open(fd: i32) -> bool {
    std::fs::metadata(format!("/dev/fd/{fd}")).is_ok()
}

#[test]
fn descriptors_above_2_are_closed_and_the_standard_ones_stay() {
    if std::env::var_os(CHILD).is_some() {
        assert!(open(0) && open(1) && open(2));
        assert!(open(9), "the test setup did not leave 9 open");
        assert!(open(10));
        close_inherited_descriptors();
        assert!(
            !open(9) && !open(10),
            "inherited descriptors are still open"
        );
        assert!(
            open(0) && open(1) && open(2),
            "a standard descriptor was closed"
        );
        return;
    }
    // a shell opens two files on high descriptors and runs this test again
    let exe = std::env::current_exe().unwrap();
    let status = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "exec 9</etc/hosts 10</etc/hosts; exec '{}' --exact \
             descriptors_above_2_are_closed_and_the_standard_ones_stay --nocapture",
            exe.display()
        ))
        .env(CHILD, "1")
        .status()
        .unwrap();
    assert!(status.success());
}
