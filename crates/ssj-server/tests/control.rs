mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, Instant};

use common::*;
use ssj_net::Identity;
use ssj_proto::bootstrap::{format_pin, parse_connect};
use ssj_proto::control::{Reply, SessionInfo, SessionState, parse_reply};
use ssj_proto::{ClientFrame, Env, ExitStatus, Hello, WindowSize};

const COUNTING_ECHO: &str =
    "stty -echo; n=0; while read l; do n=$((n+1)); echo \"got:$l:$n\"; done";

fn size() -> WindowSize {
    WindowSize::new(24, 80).unwrap()
}

fn list(runtime: &Path) -> Vec<SessionInfo> {
    let output = run_control(runtime, &["--list"], "");
    assert!(output.status.success());
    stdout_of(&output)
        .lines()
        .map(|line| match parse_reply(line.as_bytes()) {
            Ok(Reply::Info(info)) => info,
            other => panic!("unexpected list line {line:?}: {other:?}"),
        })
        .collect()
}

/// Waits until the single session is in `state`, since the state follows the events with a delay.
fn wait_state(runtime: &Path, state: SessionState) {
    let start = Instant::now();
    loop {
        let sessions = list(runtime);
        if sessions.len() == 1 && sessions[0].state == state {
            return;
        }
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "never {state:?}: {sessions:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn wait_gone(runtime: &Path) {
    let start = Instant::now();
    while !list(runtime).is_empty() {
        assert!(
            start.elapsed() < Duration::from_secs(8),
            "the session is still listed"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn socket_files(runtime: &Path) -> Vec<String> {
    std::fs::read_dir(runtime.join("ssj"))
        .map(|dir| {
            dir.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|name| name.ends_with(".sock"))
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_follows_the_state_of_the_session() {
    let runtime = runtime_dir();
    let client = Identity::generate();
    let launched = launch(&runtime, &client, free_range(), COUNTING_ECHO);

    let sessions = list(&runtime);
    assert_eq!(sessions.len(), 1);
    assert_eq!(
        (sessions[0].state, sessions[0].protocol),
        (SessionState::Disconnected, ssj_proto::PROTOCOL)
    );

    let (mut tx, mut rx, _conn) = open(&client, &launched.connect).await.unwrap();
    send(
        &mut tx,
        ClientFrame::Hello(Hello::Start {
            size: size(),
            env: Env::default(),
        }),
    )
    .await;
    send(&mut tx, ClientFrame::Input(b"x\n".to_vec())).await;
    read_until(&mut rx, |o| o.contains("got:x:1")).await;
    wait_state(&runtime, SessionState::Connected);

    send(&mut tx, ClientFrame::Detach).await;
    wait_state(&runtime, SessionState::Detached);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn new_client_attaches_to_a_detached_session_and_finds_the_shell_as_it_was() {
    let runtime = runtime_dir();
    let (first, second) = (Identity::generate(), Identity::generate());
    let launched = launch(&runtime, &first, free_range(), COUNTING_ECHO);

    let (mut tx, mut rx, _conn) = open(&first, &launched.connect).await.unwrap();
    send(
        &mut tx,
        ClientFrame::Hello(Hello::Start {
            size: size(),
            env: Env::default(),
        }),
    )
    .await;
    send(&mut tx, ClientFrame::Input(b"one\n".to_vec())).await;
    read_until(&mut rx, |o| o.contains("got:one:1")).await;
    send(&mut tx, ClientFrame::Detach).await;
    wait_state(&runtime, SessionState::Detached);
    // a detached session listens on nothing
    assert!(!reachable(&first, &launched.connect).await);

    let attach = run_control(&runtime, &["--attach"], &format_pin(&second.fingerprint()));
    assert!(
        attach.status.success(),
        "{}",
        String::from_utf8_lossy(&attach.stderr)
    );
    let fresh = parse_connect(&attach.stdout)
        .unwrap()
        .expect("a CONNECT line");
    assert_ne!(
        fresh.fingerprint, launched.connect.fingerprint,
        "a new endpoint has new certificates"
    );

    let (mut tx, mut rx, _conn) = open(&second, &fresh).await.unwrap();
    send(&mut tx, ClientFrame::Hello(Hello::Attach { size: size() })).await;
    send(&mut tx, ClientFrame::Input(b"two\n".to_vec())).await;
    // the counter of the shell is still there
    let (output, _) = read_until(&mut rx, |o| o.contains("got:two:2")).await;
    assert!(output.contains("got:two:2"), "{output:?}");

    // only the client of the attach is accepted by the new endpoint
    assert!(!reachable(&first, &fresh).await);
    // and the helper does say yes to the right client
    assert!(reachable(&second, &fresh).await);
}

#[test]
fn attach_and_kill_report_problems_instead_of_guessing() {
    let runtime = runtime_dir();
    let pin = format_pin(&Identity::generate().fingerprint());

    let none = run_control(&runtime, &["--attach"], &pin);
    assert_eq!(none.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&none.stderr).contains("no running session"));

    let launched = launch(&runtime, &Identity::generate(), free_range(), "cat");
    let _ = launched;
    let other = run_control(&runtime, &["--attach", "abcdef012345"], &pin);
    assert_eq!(other.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&other.stderr).contains("no running session abcdef012345"));

    for bad_stdin in ["", "garbage\n", "SSJ PIN abc\n"] {
        let output = run_control(&runtime, &["--attach"], bad_stdin);
        assert_eq!(output.status.code(), Some(1), "{bad_stdin:?}");
        assert!(output.stdout.is_empty(), "{bad_stdin:?}");
    }

    // an id that could point somewhere else is refused before anything happens
    let bad_id = run_control(&runtime, &["--kill", "../../etc/passwd"], "");
    assert_eq!(bad_id.status.code(), Some(2));
    assert_eq!(list(&runtime).len(), 1, "the session must still run");
}

#[test]
fn two_sessions_need_a_name() {
    let runtime = runtime_dir();
    launch(&runtime, &Identity::generate(), free_range(), "cat");
    launch(&runtime, &Identity::generate(), free_range(), "cat");
    let sessions = list(&runtime);
    assert_eq!(sessions.len(), 2);

    let unnamed = run_control(&runtime, &["--kill"], "");
    assert_eq!(unnamed.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&unnamed.stderr).contains("several sessions"));
    assert_eq!(list(&runtime).len(), 2);

    let id = sessions[0].id.as_str().to_owned();
    let named = run_control(&runtime, &["--kill", &id], "");
    assert!(named.status.success());
    wait_gone_one(&runtime, &id);
    assert_eq!(list(&runtime).len(), 1);
}

fn wait_gone_one(runtime: &Path, id: &str) {
    let start = Instant::now();
    while list(runtime).iter().any(|s| s.id.as_str() == id) {
        assert!(
            start.elapsed() < Duration::from_secs(8),
            "{id} is still listed"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill_ends_the_session_tells_the_client_and_cleans_up() {
    let runtime = runtime_dir();
    let client = Identity::generate();
    let launched = launch(&runtime, &client, free_range(), "sleep 60");
    let (mut tx, mut rx, _conn) = open(&client, &launched.connect).await.unwrap();
    send(
        &mut tx,
        ClientFrame::Hello(Hello::Start {
            size: size(),
            env: Env::default(),
        }),
    )
    .await;
    wait_state(&runtime, SessionState::Connected);

    let output = run_control(&runtime, &["--kill"], "");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(stdout_of(&output), "OK\n");

    let (_, exit) = read_until(&mut rx, |_| false).await;
    assert_eq!(exit, Some(ExitStatus::Signal(1)));
    wait_gone(&runtime);
    assert!(socket_files(&runtime).is_empty());
    assert!(!reachable(&client, &launched.connect).await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sigterm_has_the_effect_of_kill() {
    let runtime = runtime_dir();
    let launched = launch(&runtime, &Identity::generate(), free_range(), "sleep 60");
    let _ = launched;
    let sockets = socket_files(&runtime);
    assert_eq!(sockets.len(), 1);

    // the pid of the daemon, as the kernel reports it for the peer of the socket
    let stream = tokio::net::UnixStream::connect(runtime.join("ssj").join(&sockets[0]))
        .await
        .unwrap();
    let pid = stream.peer_cred().unwrap().pid().expect("a pid");
    drop(stream);
    let status = std::process::Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .unwrap();
    assert!(status.success());

    wait_gone(&runtime);
    assert!(socket_files(&runtime).is_empty());
}

#[test]
fn leftover_sockets_of_dead_servers_are_removed_and_not_listed() {
    let runtime = runtime_dir();
    let dir = runtime.join("ssj");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let dead = dir.join("abcdef012345.sock");
    drop(std::os::unix::net::UnixListener::bind(&dead).unwrap());
    assert!(dead.exists());

    assert!(list(&runtime).is_empty());

    assert!(!dead.exists());
}

#[test]
fn control_directory_and_socket_are_private() {
    let runtime = runtime_dir();
    launch(&runtime, &Identity::generate(), free_range(), "cat");

    let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&runtime.join("ssj")), 0o700);
    let sockets = socket_files(&runtime);
    assert_eq!(mode(&runtime.join("ssj").join(&sockets[0])), 0o600);
}

#[test]
fn control_directory_that_others_can_enter_is_refused() {
    let runtime = runtime_dir();
    let dir = runtime.join("ssj");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();

    let pin = format_pin(&Identity::generate().fingerprint());
    let (status, stdout, stderr, _) = run_bootstrap(&runtime, &pin, free_range(), "cat");

    assert_eq!(status.code(), Some(1));
    assert!(stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&stderr).contains("accessible to others"),
        "{}",
        String::from_utf8_lossy(&stderr)
    );
}

#[test]
fn a_runtime_directory_with_a_long_path_still_gets_a_working_socket() {
    // longer than a socket path may be
    let mut long = runtime_dir();
    for _ in 0..4 {
        long = long.join("a-long-directory-name-0123456789");
    }
    std::fs::create_dir_all(&long).unwrap();
    assert!(long.as_os_str().len() > 100);

    launch(&long, &Identity::generate(), free_range(), "cat");

    // the session is found again with the same environment, and the socket is outside the path
    assert!(!list(&long).is_empty());
    assert!(!long.join("ssj").exists());
    run_control(&long, &["--kill"], "");
}
