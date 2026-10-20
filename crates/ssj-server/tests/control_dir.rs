use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::PathBuf;

use ssj_server::control_socket::{dir_for, ensure_private_dir, host_dir_name};

fn path(text: &str) -> Option<PathBuf> {
    Some(PathBuf::from(text))
}

#[test]
fn the_runtime_directory_is_used_without_a_host_and_the_state_directory_with_one() {
    assert_eq!(
        dir_for(path("/run/user/1000"), path("/s"), path("/h"), "box", 1000).unwrap(),
        PathBuf::from("/run/user/1000/ssj")
    );
    assert_eq!(
        dir_for(None, path("/s"), path("/h"), "box", 1000).unwrap(),
        PathBuf::from("/s/ssj/run/box")
    );
    assert_eq!(
        dir_for(None, None, path("/h"), "box", 1000).unwrap(),
        PathBuf::from("/h/.local/state/ssj/run/box")
    );
    assert!(dir_for(None, None, None, "box", 1000).is_err());
}

// A shared home gives each host its own directory, so one host never takes the socket of another
// for a dead one.
#[test]
fn two_hosts_with_one_home_get_two_directories() {
    let a = dir_for(None, None, path("/home/u"), "alpha", 1000).unwrap();
    let b = dir_for(None, None, path("/home/u"), "beta", 1000).unwrap();
    assert_ne!(a, b);
}

#[test]
fn the_host_name_is_one_safe_path_component() {
    let name = host_dir_name();
    assert!(!name.is_empty() && name.len() <= 64, "{name:?}");
    assert!(!name.starts_with('.'), "{name:?}");
    assert!(
        name.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')),
        "{name:?}"
    );
}

#[test]
fn a_path_too_long_for_a_socket_moves_to_tmp_by_user() {
    // 100 bytes is the limit, and the file name takes 23.
    let fits = format!("/{}", "a".repeat(100 - 23 - "/ssj".len() - 1));
    let dir = dir_for(Some(PathBuf::from(&fits)), None, None, "box", 1000).unwrap();
    assert_eq!(dir, PathBuf::from(&fits).join("ssj"));
    assert_eq!(dir.as_os_str().len() + 23, 100);

    let too_long = format!("{fits}b");
    let dir = dir_for(Some(PathBuf::from(too_long)), None, None, "box", 1000).unwrap();
    assert_eq!(dir, PathBuf::from("/tmp/ssj-1000"));
    // the other two sources move as well.
    let long_home = PathBuf::from(format!("/{}", "h".repeat(90)));
    assert_eq!(
        dir_for(None, None, Some(long_home), "box", 7).unwrap(),
        PathBuf::from("/tmp/ssj-7")
    );
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ssjd-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn a_new_directory_is_private_and_the_check_is_repeatable() {
    let dir = scratch("new").join("a/b");
    ensure_private_dir(&dir).unwrap();
    ensure_private_dir(&dir).unwrap();
    assert_eq!(
        std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
}

#[test]
fn a_symbolic_link_in_place_of_the_directory_is_refused() {
    let base = scratch("link");
    let target = base.join("target");
    std::fs::create_dir(&target).unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700)).unwrap();
    let link = base.join("ssj");
    symlink(&target, &link).unwrap();
    let err = ensure_private_dir(&link).unwrap_err();
    assert!(err.to_string().contains("not a directory"), "{err}");
}

#[test]
fn a_file_in_place_of_the_directory_is_refused() {
    let base = scratch("file");
    let file = base.join("ssj");
    std::fs::write(&file, b"").unwrap();
    assert!(ensure_private_dir(&file).is_err());
}
