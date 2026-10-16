use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use ssj_manifest::{digest, empty_slot, hex};

fn dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(format!("/tmp/ssjt-manifest-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn tool(args: &[&Path]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ssj-manifest"))
        .args(args)
        .output()
        .unwrap()
}

fn write(path: &Path, content: &[u8]) {
    std::fs::write(path, content).unwrap();
}

fn client(dir: &Path) -> PathBuf {
    let path = dir.join("ssj");
    write(
        &path,
        &[b"program ".as_slice(), &empty_slot(), b" end"].concat(),
    );
    path
}

#[test]
fn patch_check_show_and_digest_work_on_files() {
    let dir = dir("flow");
    let (client, linux, mac) = (
        client(&dir),
        dir.join("ssj-server-x86_64-unknown-linux-musl"),
        dir.join("ssj-server-aarch64-apple-darwin"),
    );
    write(&linux, b"linux server");
    write(&mac, b"mac server");
    let before = tool(&[Path::new("digest"), &client]);
    assert!(before.status.success());

    let patched = tool(&[Path::new("patch"), &client, &linux, &mac]);
    assert!(
        patched.status.success(),
        "{}",
        String::from_utf8_lossy(&patched.stderr)
    );
    let listing = String::from_utf8_lossy(&patched.stdout).into_owned();
    assert!(
        listing.contains(&format!(
            "{}  x86_64-unknown-linux-musl",
            hex(&digest(b"linux server"))
        )),
        "{listing}"
    );

    assert!(
        tool(&[Path::new("check"), &client, &linux, &mac])
            .status
            .success()
    );
    let shown = tool(&[Path::new("show"), &client]);
    assert_eq!(String::from_utf8_lossy(&shown.stdout), listing);

    // the content digest does not move, so two builds can be compared
    let after = tool(&[Path::new("digest"), &client]);
    assert_eq!(before.stdout, after.stdout);

    // a server that changed after patching is caught
    write(&mac, b"mac server, changed");
    let check = tool(&[Path::new("check"), &client, &linux, &mac]);
    assert!(!check.status.success());
    assert!(
        String::from_utf8_lossy(&check.stderr).contains("does not hold"),
        "{}",
        String::from_utf8_lossy(&check.stderr)
    );
}

#[test]
fn bad_input_is_refused_and_leaves_the_client_alone() {
    let dir = dir("bad");
    let client = client(&dir);
    let original = std::fs::read(&client).unwrap();
    let server = dir.join("ssj-server-x86_64-unknown-linux-musl");
    write(&server, b"server");

    // not named like a server file
    let misnamed = dir.join("server");
    write(&misnamed, b"server");
    assert!(
        !tool(&[Path::new("patch"), &client, &misnamed])
            .status
            .success()
    );
    // a client without the slot, and one with two
    let plain = dir.join("plain");
    write(&plain, b"no slot");
    assert!(
        !tool(&[Path::new("patch"), &plain, &server])
            .status
            .success()
    );
    let doubled = dir.join("doubled");
    write(
        &doubled,
        &[original.clone(), empty_slot().to_vec()].concat(),
    );
    assert!(
        !tool(&[Path::new("patch"), &doubled, &server])
            .status
            .success()
    );
    // nothing to patch with, and unknown commands
    assert!(!tool(&[Path::new("patch"), &client]).status.success());
    assert!(!tool(&[Path::new("frobnicate"), &client]).status.success());

    assert_eq!(std::fs::read(&client).unwrap(), original);
}
