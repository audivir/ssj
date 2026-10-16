//! Runs the real client against the real server through a stand-in for `ssh`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Once;
use std::time::{Duration, Instant};

const FAKE_SSH: &str = "#!/bin/sh\n# usage: fake-ssh [options] -- destination command\nwhile [ \"$1\" != \"--\" ]; do shift; done\nshift\nshift\nexec sh -c \"$*\"\n";

fn server_binary() -> PathBuf {
    static BUILD: Once = Once::new();
    BUILD.call_once(|| {
        let status = Command::new(env!("CARGO"))
            .args(["build", "-p", "ssj-server"])
            .status()
            .unwrap();
        assert!(status.success());
    });
    Path::new(env!("CARGO_BIN_EXE_ssj")).with_file_name("ssj-server")
}

fn fake_ssh(script: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let dir =
        std::env::temp_dir().join(format!("ssj-e2e-{}-{}", std::process::id(), rand_suffix()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("fake-ssh");
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn rand_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

fn free_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Runs `ssj` with `input` on stdin and a shell command for the server, and waits for it.
fn run_client(ssh: &Path, shell_args: &[&str], input: &str) -> Output {
    let port = free_port();
    let mut command = Command::new(env!("CARGO_BIN_EXE_ssj"));
    command
        .args(["--ssh-command", ssh.to_str().unwrap()])
        .args(["--server-path", server_binary().to_str().unwrap()])
        .args([
            "--udp-host",
            "127.0.0.1",
            "--port-range",
            &format!("{port}:{port}"),
        ])
        .args([
            "--detached-timeout",
            "5",
            "--server-arg",
            "--shell",
            "--server-arg",
            "/bin/sh",
        ]);
    for arg in shell_args {
        command.args(["--server-arg", "--shell-arg", "--server-arg", arg]);
    }
    let mut child = command
        .arg("user@localhost")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();

    let start = Instant::now();
    loop {
        if child.try_wait().unwrap().is_some() {
            return child.wait_with_output().unwrap();
        }
        if start.elapsed() > Duration::from_secs(60) {
            child.kill().unwrap();
            panic!(
                "the client did not finish: {:?}",
                child.wait_with_output().unwrap()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn shell_command_runs_through_bootstrap_quic_and_pty_and_its_exit_code_is_returned() {
    let output = run_client(
        &fake_ssh(FAKE_SSH),
        &["-c", "read x; echo got:$x; exit 3"],
        "xyz\n",
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        stdout.contains("got:xyz"),
        "stdout: {stdout:?} stderr: {stderr:?}"
    );
    assert_eq!(output.status.code(), Some(3), "stderr: {stderr:?}");
}

#[test]
fn piped_script_runs_in_an_interactive_shell() {
    let output = run_client(
        &fake_ssh(FAKE_SSH),
        &["-i"],
        "echo hello-from-shell\nexit 5\n",
    );
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(stdout.contains("hello-from-shell"), "{stdout:?}");
    assert_eq!(output.status.code(), Some(5));
}

// Text from the remote side may carry terminal escape sequences and must not reach the screen.
#[test]
fn failed_bootstrap_shows_remote_text_without_control_characters() {
    let ssh =
        fake_ssh("#!/bin/sh\nprintf '\\033[2J\\033]0;pwned\\007remote said no\\n' >&2\nexit 1\n");
    let output = run_client(&ssh, &["-c", "true"], "");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(1));
    assert!(stderr.contains("remote said no"), "{stderr:?}");
    assert!(
        !stderr.chars().any(|c| c.is_control() && c != '\n'),
        "{stderr:?}"
    );
}

/// Runs the client on a pseudo-terminal, as a user would, and returns what the terminal shows.
#[test]
fn client_on_a_terminal_passes_the_window_size_and_exits_without_hanging() {
    use std::io::{Read, Write};
    use std::sync::mpsc;

    let ssh = fake_ssh(FAKE_SSH);
    let port = free_port();
    let (pty, pts) = pty_process::blocking::open().unwrap();
    pty.resize(pty_process::Size::new(31, 97)).unwrap();
    let mut child = pty_process::blocking::Command::new(env!("CARGO_BIN_EXE_ssj"))
        .args(["--ssh-command", ssh.to_str().unwrap()])
        .args(["--server-path", server_binary().to_str().unwrap()])
        .args([
            "--udp-host",
            "127.0.0.1",
            "--port-range",
            &format!("{port}:{port}"),
        ])
        .args([
            "--detached-timeout",
            "5",
            "--server-arg",
            "--shell",
            "--server-arg",
            "/bin/sh",
        ])
        .args([
            "--server-arg",
            "--shell-arg",
            "--server-arg",
            "-i",
            "me@localhost",
        ])
        .spawn(pts)
        .unwrap();

    // one handle to read on a thread and one to write
    let fd: std::os::fd::OwnedFd = pty.into();
    let mut reader = std::fs::File::from(fd.try_clone().unwrap());
    let mut pty = std::fs::File::from(fd);
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });
    let mut seen = String::new();
    let mut wait_for = |needle: &str| {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !seen.contains(needle) {
            let left = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(left) {
                Ok(bytes) => seen.push_str(&String::from_utf8_lossy(&bytes)),
                Err(_) => panic!("never saw {needle:?} in {seen:?}"),
            }
        }
    };

    wait_for("$");
    pty.write_all(b"stty size\n").unwrap();
    wait_for("31 97");
    pty.write_all(b"exit 7\n").unwrap();

    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "the client hangs after the shell exited"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(status.code(), Some(7));
}

fn short_dir() -> PathBuf {
    let dir = PathBuf::from(format!(
        "/tmp/ssjt-c-{}-{}",
        std::process::id(),
        rand_suffix() % 1_000_000
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Runs `ssj` with the given words and stdin, against the server in the same runtime directory.
fn ssj(runtime: &Path, ssh: &Path, words: &[&str], input: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_ssj"))
        .env("XDG_RUNTIME_DIR", runtime)
        .args(["--ssh-command", ssh.to_str().unwrap()])
        .args(["--server-path", server_binary().to_str().unwrap()])
        .args(["--udp-host", "127.0.0.1"])
        .args(words)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let start = Instant::now();
    loop {
        if child.try_wait().unwrap().is_some() {
            return child.wait_with_output().unwrap();
        }
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "ssj {words:?} did not finish"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Types `send` into `ssj` and waits until its output contains `expect` before the next step,
/// like a person who reads the screen. Returns the output when the client has ended.
fn ssj_typing(runtime: &Path, ssh: &Path, words: &[&str], steps: &[(&str, &str)]) -> Output {
    use std::io::Read;
    use std::sync::{Arc, Mutex};

    let mut child = Command::new(env!("CARGO_BIN_EXE_ssj"))
        .env("XDG_RUNTIME_DIR", runtime)
        .args(["--ssh-command", ssh.to_str().unwrap()])
        .args(["--server-path", server_binary().to_str().unwrap()])
        .args(["--udp-host", "127.0.0.1"])
        .args(words)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let seen = Arc::new(Mutex::new(String::new()));
    let mut stdout = child.stdout.take().unwrap();
    let collector = seen.clone();
    let reader = std::thread::spawn(move || {
        let mut buf = [0u8; 1024];
        while let Ok(n) = stdout.read(&mut buf) {
            if n == 0 {
                break;
            }
            collector
                .lock()
                .unwrap()
                .push_str(&String::from_utf8_lossy(&buf[..n]));
        }
    });
    let mut stdin = child.stdin.take().unwrap();
    for (send, expect) in steps {
        stdin.write_all(send.as_bytes()).unwrap();
        let start = Instant::now();
        while !seen.lock().unwrap().contains(expect) {
            assert!(
                start.elapsed() < Duration::from_secs(30),
                "never saw {expect:?} in {:?}",
                seen.lock().unwrap()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    let start = Instant::now();
    while child.try_wait().unwrap().is_none() {
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "ssj {words:?} did not finish"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = reader.join();
    let mut output = child.wait_with_output().unwrap();
    output.stdout = seen.lock().unwrap().clone().into_bytes();
    output
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn list_attach_detach_attach_kill_through_the_client() {
    use ssj_proto::bootstrap::format_pin;

    let runtime = short_dir();
    let ssh = fake_ssh(FAKE_SSH);
    let script = "stty -echo; n=0; while read l; do n=$((n+1)); echo \"got:$l:$n\"; done";

    // a session whose first client never shows up, as after a crash of the client
    let port = free_port();
    let mut daemon = Command::new(server_binary())
        .env("XDG_RUNTIME_DIR", &runtime)
        .args([
            "--port-range",
            &format!("{port}:{port}"),
            "--detached-timeout",
            "60",
        ])
        .args([
            "--shell",
            "/bin/sh",
            "--shell-arg",
            "-c",
            "--shell-arg",
            script,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let dummy = ssj_net::Identity::generate();
    daemon
        .stdin
        .take()
        .unwrap()
        .write_all(format_pin(&dummy.fingerprint()).as_bytes())
        .unwrap();
    assert!(daemon.wait_with_output().unwrap().status.success());

    let listed = ssj(&runtime, &ssh, &["list", "user@localhost"], "");
    let listing = text(&listed.stdout);
    assert!(listed.status.success(), "{}", text(&listed.stderr));
    assert!(
        listing.contains("disconnected") && listing.contains("protocol 1"),
        "{listing}"
    );
    let id = listing.split_whitespace().next().unwrap().to_owned();

    // the first attach types a line and leaves with the escape
    let first = ssj_typing(
        &runtime,
        &ssh,
        &["attach", &id, "user@localhost"],
        &[
            (
                "hello
",
                "got:hello:1",
            ),
            ("~.", ""),
        ],
    );
    assert!(
        text(&first.stdout).contains("got:hello:1"),
        "{} / {}",
        text(&first.stdout),
        text(&first.stderr)
    );
    assert_eq!(first.status.code(), Some(0));
    assert!(
        text(&first.stderr).contains("detached"),
        "{}",
        text(&first.stderr)
    );

    // the second one finds the shell as it was
    let second = ssj_typing(
        &runtime,
        &ssh,
        &["attach", "user@localhost"],
        &[("again\n", "got:again:2"), ("~.", "")],
    );
    assert!(
        text(&second.stdout).contains("got:again:2"),
        "{} / {}",
        text(&second.stdout),
        text(&second.stderr)
    );

    let listing = text(&ssj(&runtime, &ssh, &["list", "user@localhost"], "").stdout);
    assert!(listing.contains("detached"), "{listing}");

    let killed = ssj(&runtime, &ssh, &["kill", "user@localhost"], "");
    assert!(killed.status.success(), "{}", text(&killed.stderr));
    assert!(text(&killed.stdout).contains("session ended"));
    let start = Instant::now();
    loop {
        let listing = text(&ssj(&runtime, &ssh, &["list", "user@localhost"], "").stdout);
        if listing.contains("no sessions") {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(8),
            "still listed: {listing}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn attach_and_kill_without_a_session_say_so() {
    let runtime = short_dir();
    let ssh = fake_ssh(FAKE_SSH);
    for words in [
        vec!["attach", "user@localhost"],
        vec!["kill", "user@localhost"],
        vec!["kill", "abcdef012345", "user@localhost"],
    ] {
        let output = ssj(&runtime, &ssh, &words, "");
        assert_eq!(output.status.code(), Some(1), "{words:?}");
        assert!(
            text(&output.stderr).contains("no running session"),
            "{words:?}: {}",
            text(&output.stderr)
        );
    }
    let listed = ssj(&runtime, &ssh, &["list", "user@localhost"], "");
    assert!(text(&listed.stdout).contains("no sessions"));
}
