use std::time::Duration;

use ssj_client::run::sessions_json;
use ssj_client::sessions_cache::{Origin, Refresh, names, stale_origins, store};
use ssj_proto::SessionId;
use ssj_proto::control::{SessionInfo, SessionState};

fn scratch(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("ssj-cache-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn origin(destination: &str, port: Option<u16>) -> Origin {
    Origin {
        destination: destination.to_owned(),
        port,
    }
}

#[test]
fn sessions_are_listed_as_json() {
    let info = SessionInfo {
        id: SessionId::parse("abcdefgh12345678").unwrap(),
        name: ssj_proto::SessionName::parse("work"),
        state: SessionState::Detached,
        age_secs: 7,
        protocol: 1,
    };
    let unnamed = SessionInfo {
        name: None,
        state: SessionState::Connected,
        ..info.clone()
    };
    let text = sessions_json(&[info.clone(), unnamed.clone()]);
    assert_eq!(
        text,
        format!(
            "[{{\"id\":\"{0}\",\"name\":\"work\",\"state\":\"detached\",\"age_secs\":7,\"protocol\":1}},\
             {{\"id\":\"{0}\",\"name\":null,\"state\":\"connected\",\"age_secs\":7,\"protocol\":1}}]",
            info.id.as_str()
        )
    );
    assert_eq!(sessions_json(&[]), "[]");
}

#[test]
fn the_cache_keeps_names_per_host_and_replaces_them() {
    let dir = scratch("names");
    let a = origin("me@a", None);
    let b = origin("b", Some(2222));
    store(&dir, &a, &["work".into(), "abc".into()]).unwrap();
    store(&dir, &b, &["work".into(), "xyz".into()]).unwrap();
    assert_eq!(names(&dir, Some("me@a")), ["work", "abc"]);
    assert_eq!(names(&dir, Some("b")), ["work", "xyz"]);
    // all hosts, without repeats
    let mut all = names(&dir, None);
    all.sort();
    assert_eq!(all, ["abc", "work", "xyz"]);
    store(&dir, &a, &[]).unwrap();
    assert!(names(&dir, Some("me@a")).is_empty());
    assert!(names(&dir, Some("nobody")).is_empty());
    // the directory is private
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&dir).unwrap().permissions().mode();
    assert_eq!(mode & 0o077, 0, "{mode:o}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn old_lists_are_found_with_where_they_came_from() {
    let dir = scratch("stale");
    store(&dir, &origin("me@a", Some(2222)), &["x".into()]).unwrap();
    assert!(stale_origins(&dir, Duration::from_secs(3600)).is_empty());
    assert_eq!(
        stale_origins(&dir, Duration::ZERO),
        [origin("me@a", Some(2222))]
    );
    // a hand-edited file must not smuggle anything into the `ssh` command line
    std::fs::write(dir.join("evil.list"), "@-oProxyCommand=id\nx\n").unwrap();
    std::fs::write(dir.join("evil2.list"), "@a;id\n").unwrap();
    assert_eq!(stale_origins(&dir, Duration::ZERO).len(), 1);
    // and a name that is no selector is not offered
    std::fs::write(dir.join("evil3.list"), "@h\n$(id)\nok\n").unwrap();
    assert!(!names(&dir, None).iter().any(|n| n.contains('$')));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn one_refresh_per_host_runs_at_a_time() {
    let dir = scratch("lock");
    let first = Refresh::acquire(&dir, "me@a").expect("free");
    assert!(Refresh::acquire(&dir, "me@a").is_none());
    assert!(Refresh::acquire(&dir, "other").is_some());
    drop(first);
    assert!(Refresh::acquire(&dir, "me@a").is_some());
    std::fs::remove_dir_all(&dir).unwrap();
}
