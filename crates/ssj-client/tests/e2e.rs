//! Runs the real client against the real server through a stand-in for `ssh`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Once;
use std::time::{Duration, Instant};

const FAKE_SSH: &str = "#!/bin/sh\n# usage: fake-ssh [options] -- destination command\nwhile [ \"$1\" != \"--\" ]; do shift; done\nshift\nshift\nexec sh -c \"$*\"\n";

/// Starts processes one at a time.
///
/// A test binary starts processes from several threads. On macOS a pipe is created and then marked
/// as not inheritable in a second step, and a process that another thread starts in between
/// inherits the pipe. The reading side of the test then waits for the end of a pipe that an
/// unrelated daemon keeps open. Taking a lock around the start avoids that.
pub trait SpawnLocked {
    fn spawn_locked(&mut self) -> std::io::Result<std::process::Child>;
}

impl SpawnLocked for std::process::Command {
    fn spawn_locked(&mut self) -> std::io::Result<std::process::Child> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        self.spawn()
    }
}

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

/// A number that no other test of this process gets. The clock is not enough: on macOS it counts
/// microseconds, and two tests that start together would write their files into the same place.
fn rand_suffix() -> u128 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    u128::from(COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// A UDP port for a server that is free now.
///
/// The ports come from a range below the ephemeral ports of macOS (from 49152) and of Linux (from
/// 32768). A port from the ephemeral range can be taken at any moment by the client socket of
/// another test, and then the packets for the server go to the wrong socket and a handshake waits
/// forever. Each test process starts at a different place of the range.
fn free_port() -> u16 {
    use std::net::UdpSocket;
    use std::sync::atomic::{AtomicU16, Ordering};

    static NEXT: AtomicU16 = AtomicU16::new(0);
    let base = 20_000 + (std::process::id() % 400) as u16 * 25;
    loop {
        let port = base + NEXT.fetch_add(1, Ordering::Relaxed) % 5_000;
        // free on both families, since the server listens on a dual stack socket
        let v4 = UdpSocket::bind(("0.0.0.0", port));
        let v6 = UdpSocket::bind(("::", port));
        if v4.is_ok() && v6.is_ok() {
            return port;
        }
    }
}

/// Runs `ssj` with `input` on stdin and a shell command for the server, and waits for it.
fn run_client(ssh: &Path, shell_args: &[&str], input: &str) -> Output {
    run_client_with(ssh, shell_args, input, &[])
}

/// Like [`run_client`] with more arguments for the client.
fn run_client_with(ssh: &Path, shell_args: &[&str], input: &str, client_args: &[&str]) -> Output {
    run_client_to(
        ssh,
        shell_args,
        input,
        client_args,
        "user@localhost",
        Some("127.0.0.1"),
    )
}

/// Like [`run_client_with`] for another destination, and with or without the UDP address.
fn run_client_to(
    ssh: &Path,
    shell_args: &[&str],
    input: &str,
    client_args: &[&str],
    destination: &str,
    udp_host: Option<&str>,
) -> Output {
    let port = free_port();
    let mut command = Command::new(env!("CARGO_BIN_EXE_ssj"));
    command
        .args(["--ssh-command", ssh.to_str().unwrap()])
        .args(["--server-path", server_binary().to_str().unwrap()])
        .args(["--port-range", &format!("{port}:{port}")]);
    if let Some(host) = udp_host {
        command.args(["--udp-host", host]);
    }
    command.args([
        "--detached-timeout",
        "5",
        "--server-arg",
        "--max-detached",
        "--server-arg",
        "30",
        "--server-arg",
        "--shell",
        "--server-arg",
        "/bin/sh",
    ]);
    for arg in shell_args {
        command.args(["--server-arg", "--shell-arg", "--server-arg", arg]);
    }
    let mut child = command
        .args(client_args)
        .arg(destination)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn_locked()
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
fn every_ssh_that_runs_a_command_gets_no_escape_character() {
    let log = fake_ssh("").with_file_name("args.log");
    let script = format!(
        "#!/bin/sh\necho \"$*\" >> {}\n{}",
        log.display(),
        &FAKE_SSH["#!/bin/sh\n".len()..]
    );
    let output = run_client(&fake_ssh(&script), &["-c", "exit 0"], "");
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let logged = std::fs::read_to_string(&log).unwrap();
    let calls: Vec<&str> = logged.lines().collect();
    assert!(!calls.is_empty());
    for call in calls {
        // only the query of the configuration (`-G`) does not open a session
        if call.split(' ').any(|word| word == "-G") {
            continue;
        }
        let before_destination = call.split(" -- ").next().unwrap();
        assert!(before_destination.contains("-e none"), "{call}");
    }
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
            "--max-detached",
            "--server-arg",
            "30",
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
        rand_suffix()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Runs `ssj` with the given words and stdin, against the server in the same runtime directory.
fn ssj(runtime: &Path, ssh: &Path, words: &[&str], input: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_ssj"))
        .env("XDG_RUNTIME_DIR", runtime)
        .env("XDG_CACHE_HOME", runtime)
        .args(["--ssh-command", ssh.to_str().unwrap()])
        .args(["--server-path", server_binary().to_str().unwrap()])
        .args(["--udp-host", "127.0.0.1"])
        .args(words)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn_locked()
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
        .env("XDG_CACHE_HOME", runtime)
        .args(["--ssh-command", ssh.to_str().unwrap()])
        .args(["--server-path", server_binary().to_str().unwrap()])
        .args(["--udp-host", "127.0.0.1"])
        .args(words)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn_locked()
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
        .spawn_locked()
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

    // the same list as JSON, and the cache that completion reads
    let json = ssj(&runtime, &ssh, &["list", "--json", "user@localhost"], "");
    let json = text(&json.stdout);
    assert!(
        json.starts_with("[{\"id\":\"") && json.contains("\"state\":\"disconnected\""),
        "{json}"
    );
    let cached = ssj(&runtime, &ssh, &["--complete", "user@localhost"], "");
    assert!(text(&cached.stdout).lines().any(|l| l == id), "{cached:?}");

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

/// The name of the server build that matches the system of the tests, which is also the "host".
fn local_build() -> &'static str {
    let output = Command::new("uname").arg("-sm").output().unwrap();
    match String::from_utf8_lossy(&output.stdout).trim() {
        "Darwin arm64" => "aarch64-apple-darwin",
        "Linux x86_64" => "x86_64-unknown-linux-musl",
        "Linux aarch64" => "aarch64-unknown-linux-musl",
        other => panic!("the tests have no server build for {other}"),
    }
}

/// A directory with the server file under the name that the client looks for.
fn server_dir() -> PathBuf {
    let dir = short_dir();
    std::fs::copy(
        server_binary(),
        dir.join(format!("ssj-server-{}", local_build())),
    )
    .unwrap();
    dir
}

/// Directories for one test host: where the data (installed files) and the sockets go.
struct Host {
    data: PathBuf,
    config: PathBuf,
    runtime: PathBuf,
}

fn host() -> Host {
    Host {
        data: short_dir(),
        config: short_dir(),
        runtime: short_dir(),
    }
}

impl Host {
    fn installed(&self) -> PathBuf {
        self.data.join("ssj/bin/ssj-server-p1")
    }

    /// Runs `ssj` as a script would, without a terminal, with the words as given.
    fn ssj(&self, ssh: &Path, words: &[&str], input: &str) -> Output {
        self.ssj_with(Path::new(env!("CARGO_BIN_EXE_ssj")), ssh, words, input)
    }

    /// Like [`Host::ssj`] with another build of the client.
    fn ssj_with(&self, exe: &Path, ssh: &Path, words: &[&str], input: &str) -> Output {
        let mut child = Command::new(exe)
            .env("XDG_DATA_HOME", &self.data)
            .env("XDG_CONFIG_HOME", &self.config)
            .env("XDG_RUNTIME_DIR", &self.runtime)
            .env("XDG_CACHE_HOME", &self.runtime)
            .args([
                "--ssh-command",
                ssh.to_str().unwrap(),
                "--udp-host",
                "127.0.0.1",
            ])
            .args(words)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn_locked()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let start = Instant::now();
        while child.try_wait().unwrap().is_none() {
            assert!(
                start.elapsed() < Duration::from_secs(60),
                "ssj {words:?} did not finish"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        child.wait_with_output().unwrap()
    }

    /// The words that start a short session without naming the server file.
    fn session_words(&self, extra: &[&str]) -> Vec<String> {
        let port = free_port();
        let mut words: Vec<String> = extra.iter().map(|w| (*w).to_owned()).collect();
        for arg in [
            "--port-range",
            &format!("{port}:{port}"),
            "--detached-timeout",
            "5",
            "--server-arg",
            "--max-detached",
            "--server-arg",
            "30",
            "--server-arg",
            "--shell",
            "--server-arg",
            "/bin/sh",
            "--server-arg",
            "--shell-arg",
            "--server-arg",
            "-c",
            "--server-arg",
            "--shell-arg",
            "--server-arg",
            "read x; echo got:$x; exit 3",
        ] {
            words.push(arg.to_owned());
        }
        words.push("user@localhost".to_owned());
        words
    }
}

fn words(owned: &[String]) -> Vec<&str> {
    owned.iter().map(String::as_str).collect()
}

#[test]
fn install_uploads_checks_and_makes_the_default_path_work() {
    let host = host();
    let ssh = fake_ssh(FAKE_SSH);
    let dir = server_dir();

    let output = host.ssj(
        &ssh,
        &[
            "install",
            "--server-dir",
            dir.to_str().unwrap(),
            "user@localhost",
        ],
        "",
    );
    assert!(output.status.success(), "{}", text(&output.stderr));
    assert!(
        text(&output.stdout).contains("installed "),
        "{}",
        text(&output.stdout)
    );

    // a private, executable file that reports the protocol
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(host.installed())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o700);
    let version = Command::new(host.installed())
        .arg("--version")
        .output()
        .unwrap();
    assert!(text(&version.stdout).contains("protocol 1"));
    // no temporary files stay behind
    let names: Vec<String> = std::fs::read_dir(host.data.join("ssj/bin"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["ssj-server-p1"]);

    // and a session starts through the default path, without --server-path
    let session = host.session_words(&[]);
    let output = host.ssj(&ssh, &words(&session), "go\n");
    assert!(
        text(&output.stdout).contains("got:go"),
        "{} / {}",
        text(&output.stdout),
        text(&output.stderr)
    );
    assert_eq!(output.status.code(), Some(3));
}

#[test]
fn missing_server_is_installed_on_demand_with_the_flag() {
    let host = host();
    let ssh = fake_ssh(FAKE_SSH);
    let dir = server_dir();
    let session = host.session_words(&["--install", "--server-dir", dir.to_str().unwrap()]);

    let output = host.ssj(&ssh, &words(&session), "go\n");

    assert!(
        text(&output.stderr).contains("installed "),
        "{}",
        text(&output.stderr)
    );
    assert!(
        text(&output.stdout).contains("got:go"),
        "{} / {}",
        text(&output.stdout),
        text(&output.stderr)
    );
    assert_eq!(output.status.code(), Some(3));
    assert!(host.installed().exists());
}

#[test]
fn without_a_terminal_a_missing_server_is_not_installed_unasked() {
    let host = host();
    let ssh = fake_ssh(FAKE_SSH);
    let dir = server_dir();
    let session = host.session_words(&["--server-dir", dir.to_str().unwrap()]);

    let output = host.ssj(&ssh, &words(&session), "");

    assert_eq!(output.status.code(), Some(1));
    let stderr = text(&output.stderr);
    assert!(
        stderr.contains("not installed")
            && stderr.contains("no terminal")
            && stderr.contains("--install"),
        "{stderr}"
    );
    assert!(!host.installed().exists());
}

#[test]
fn the_config_file_and_the_flags_decide_in_this_order() {
    let host = host();
    let ssh = fake_ssh(FAKE_SSH);
    let dir = server_dir();
    std::fs::create_dir_all(host.config.join("ssj")).unwrap();
    let config = host.config.join("ssj/client.toml");
    let write = |text: &str| {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(&config, text).unwrap();
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
    };

    // "never" in the config stops an installation that would otherwise happen
    write("auto_install = \"never\"\n");
    let session = host.session_words(&["--server-dir", dir.to_str().unwrap()]);
    let output = host.ssj(&ssh, &words(&session), "go\n");
    assert_eq!(output.status.code(), Some(1));
    assert!(
        text(&output.stderr).contains("ssj install"),
        "{}",
        text(&output.stderr)
    );
    assert!(!host.installed().exists());

    // the flag wins over the config
    let session = host.session_words(&["--install", "--server-dir", dir.to_str().unwrap()]);
    assert_eq!(
        host.ssj(&ssh, &words(&session), "go\n").status.code(),
        Some(3)
    );
    assert!(host.installed().exists());
    std::fs::remove_file(host.installed()).unwrap();

    // "always" in the config installs without a flag
    write("auto_install = \"always\"\n");
    let session = host.session_words(&["--server-dir", dir.to_str().unwrap()]);
    assert_eq!(
        host.ssj(&ssh, &words(&session), "go\n").status.code(),
        Some(3)
    );
    assert!(host.installed().exists());
    std::fs::remove_file(host.installed()).unwrap();

    // and --no-install wins over "always"
    let session = host.session_words(&["--no-install", "--server-dir", dir.to_str().unwrap()]);
    assert_eq!(
        host.ssj(&ssh, &words(&session), "go\n").status.code(),
        Some(1)
    );
    assert!(!host.installed().exists());

    // a broken config is an error, not a silent default
    write("auto_instal = \"always\"\n");
    let output = host.ssj(&ssh, &["list", "user@localhost"], "");
    assert_eq!(output.status.code(), Some(2));
    assert!(
        text(&output.stderr).contains("unknown key"),
        "{}",
        text(&output.stderr)
    );
}

// A transfer that is changed on the way must never replace or create the server file.
#[test]
fn damaged_upload_is_rejected_and_installs_nothing() {
    let host = host();
    let ssh = fake_ssh(
        "#!/bin/sh\nwhile [ \"$1\" != \"--\" ]; do shift; done\nshift\nshift\n{ cat; printf x; } | sh -c \"$*\"\n",
    );
    let dir = server_dir();

    let output = host.ssj(
        &ssh,
        &[
            "install",
            "--server-dir",
            dir.to_str().unwrap(),
            "user@localhost",
        ],
        "",
    );

    assert_eq!(output.status.code(), Some(1));
    assert!(
        text(&output.stderr).contains("does not match its digest"),
        "{}",
        text(&output.stderr)
    );
    assert!(!host.installed().exists());
    let leftovers = std::fs::read_dir(host.data.join("ssj/bin"))
        .map(|d| d.count())
        .unwrap_or(0);
    assert_eq!(leftovers, 0, "the temporary file must be removed");
}

#[test]
fn unsupported_systems_and_missing_builds_are_explained() {
    let host = host();
    let freebsd = fake_ssh("#!/bin/sh\necho 'SSJ TARGET FreeBSD amd64'\n");
    let dir = server_dir();
    let output = host.ssj(
        &freebsd,
        &[
            "install",
            "--server-dir",
            dir.to_str().unwrap(),
            "user@localhost",
        ],
        "",
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(
        text(&output.stderr).contains("no server for the system"),
        "{}",
        text(&output.stderr)
    );

    let ssh = fake_ssh(FAKE_SSH);
    let empty = short_dir();
    let output = host.ssj(
        &ssh,
        &[
            "install",
            "--server-dir",
            empty.to_str().unwrap(),
            "user@localhost",
        ],
        "",
    );
    assert_eq!(output.status.code(), Some(1));
    let stderr = text(&output.stderr);
    assert!(
        stderr.contains(&format!("ssj-server-{}", local_build()))
            && stderr.contains("--server-dir"),
        "{stderr}"
    );
}

#[test]
fn listing_and_attaching_to_a_host_without_the_server_say_so() {
    let host = host();
    let ssh = fake_ssh(FAKE_SSH);

    let listed = host.ssj(&ssh, &["list", "user@localhost"], "");
    assert_eq!(listed.status.code(), Some(0));
    assert!(
        text(&listed.stdout).contains("not installed"),
        "{}",
        text(&listed.stdout)
    );

    for command in ["attach", "kill"] {
        let output = host.ssj(&ssh, &[command, "user@localhost"], "");
        assert_eq!(output.status.code(), Some(1), "{command}");
        assert!(
            text(&output.stderr).contains("not installed"),
            "{command}: {}",
            text(&output.stderr)
        );
        assert!(!host.installed().exists(), "{command} must never install");
    }
}

/// Starts `ssj` on a pseudo-terminal, so that it can ask questions, and follows what it shows.
struct OnTerminal {
    child: std::process::Child,
    typed: std::fs::File,
    shown: std::sync::mpsc::Receiver<Vec<u8>>,
    seen: String,
}

fn on_terminal(host: &Host, ssh: &Path, words: &[String]) -> OnTerminal {
    use std::io::Read;

    let (pty, pts) = pty_process::blocking::open().unwrap();
    pty.resize(pty_process::Size::new(24, 80)).unwrap();
    let child = pty_process::blocking::Command::new(env!("CARGO_BIN_EXE_ssj"))
        .env("XDG_DATA_HOME", &host.data)
        .env("XDG_CONFIG_HOME", &host.config)
        .env("XDG_RUNTIME_DIR", &host.runtime)
        .env("XDG_CACHE_HOME", &host.runtime)
        .args([
            "--ssh-command",
            ssh.to_str().unwrap(),
            "--udp-host",
            "127.0.0.1",
        ])
        .args(words)
        .spawn(pts)
        .unwrap();
    let fd: std::os::fd::OwnedFd = pty.into();
    let mut reader = std::fs::File::from(fd.try_clone().unwrap());
    let typed = std::fs::File::from(fd);
    let (tx, shown) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });
    OnTerminal {
        child,
        typed,
        shown,
        seen: String::new(),
    }
}

impl OnTerminal {
    fn wait_for(&mut self, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !self.seen.contains(needle) {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.shown.recv_timeout(left) {
                Ok(bytes) => self.seen.push_str(&String::from_utf8_lossy(&bytes)),
                Err(_) => panic!("never saw {needle:?} in {:?}", self.seen),
            }
        }
    }

    fn exit_code(&mut self) -> Option<i32> {
        let start = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status.code();
            }
            assert!(
                start.elapsed() < Duration::from_secs(30),
                "the client did not end: {:?}",
                self.seen
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

#[test]
fn on_a_terminal_the_user_is_asked_and_can_decline() {
    let host = host();
    let ssh = fake_ssh(FAKE_SSH);
    let dir = server_dir();
    let mut terminal = on_terminal(
        &host,
        &ssh,
        &host.session_words(&["--server-dir", dir.to_str().unwrap()]),
    );

    terminal.wait_for("Install it now? [y/N]");
    terminal.typed.write_all(b"n\n").unwrap();

    assert_eq!(terminal.exit_code(), Some(1));
    assert!(!host.installed().exists());
}

#[test]
fn on_a_terminal_the_user_can_agree_and_the_session_starts() {
    let host = host();
    let ssh = fake_ssh(FAKE_SSH);
    let dir = server_dir();
    let mut terminal = on_terminal(
        &host,
        &ssh,
        &host.session_words(&["--server-dir", dir.to_str().unwrap()]),
    );

    terminal.wait_for("Install it now? [y/N]");
    terminal.typed.write_all(b"y\n").unwrap();
    terminal.wait_for("installed ");
    terminal.typed.write_all(b"go\n").unwrap();
    terminal.wait_for("got:go");

    assert_eq!(terminal.exit_code(), Some(3));
    assert!(host.installed().exists());
}

// Only a clear yes counts, so that a stray key or an empty answer never uploads anything.
#[test]
fn only_yes_means_yes() {
    for answer in ["\n", "yep\n", "no\n", "n\n", " \n"] {
        let host = host();
        let ssh = fake_ssh(FAKE_SSH);
        let dir = server_dir();
        let mut terminal = on_terminal(
            &host,
            &ssh,
            &host.session_words(&["--server-dir", dir.to_str().unwrap()]),
        );
        terminal.wait_for("[y/N]");
        terminal.typed.write_all(answer.as_bytes()).unwrap();
        assert_eq!(terminal.exit_code(), Some(1), "{answer:?}");
        assert!(!host.installed().exists(), "{answer:?}");
    }
}

/// A stand-in for `ssh` that also does what the transports need: `-W host:port` joins stdin and
/// stdout to a TCP connection, and `-G` prints a configuration. `extra` runs first, with `$1` as
/// the first argument, and may end the script.
fn transport_ssh(config: &str, extra: &str) -> PathBuf {
    let relay = "import os, socket, sys, threading\n\
host, port = sys.argv[1].rsplit(':', 1)\n\
s = socket.create_connection((host, int(port)))\n\
def up():\n    while True:\n        d = os.read(0, 65536)\n        if not d:\n            break\n        s.sendall(d)\n    s.shutdown(socket.SHUT_WR)\n\
threading.Thread(target=up, daemon=True).start()\n\
while True:\n    d = s.recv(65536)\n    if not d:\n        break\n    os.write(1, d)\n";
    let relay_path = fake_ssh("").with_file_name("relay.py");
    std::fs::write(&relay_path, relay).unwrap();
    fake_ssh(&format!(
        "#!/bin/sh\n\
forward=\n\
case \"$*\" in *-G*) printf '%s' '{config}'; exit 0;; esac\n\
for a in \"$@\"; do [ \"$a\" = -W ] && want=1; done\n\
while [ \"$1\" != \"--\" ]; do\n\
  if [ \"$1\" = -W ]; then shift; forward=\"$1\"; fi\n\
  shift\n\
done\n\
shift; shift\n\
{extra}\n\
if [ -n \"$forward\" ]; then exec python3 -I {} \"$forward\"; fi\n\
exec sh -c \"$*\"\n",
        relay_path.display()
    ))
}

const NO_UDP: [&str; 2] = ["--server-arg", "--no-udp"];

fn assert_session(output: &Output) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains("got:xyz"),
        "stdout: {stdout:?} stderr: {stderr:?}"
    );
    assert_eq!(output.status.code(), Some(3), "stderr: {stderr:?}");
}

const ASK: [&str; 2] = ["-c", "read x; echo got:$x; exit 3"];

#[test]
fn tcp_through_ssh_w_carries_a_session() {
    let ssh = transport_ssh("", "");
    let output = run_client_with(&ssh, &ASK, "xyz\n", &["--transport", "tcp"]);
    assert_session(&output);
}

#[test]
fn the_ssh_channel_alone_carries_a_session() {
    let ssh = transport_ssh("", "");
    let output = run_client_with(
        &ssh,
        &ASK,
        "xyz\n",
        &[
            "--transport",
            "stdio",
            "--server-arg",
            "--no-udp",
            "--server-arg",
            "--no-tcp",
        ],
    );
    assert_session(&output);
}

// Shell startup files print text before the stream, and the client must find the start of it.
#[test]
fn text_printed_before_the_stream_is_skipped() {
    let ssh = transport_ssh(
        "",
        "case \"$*\" in *--stdio*) echo 'Welcome to the host'; echo 'SSJ CONNECT noise'; printf '\\033[2Jx';; esac",
    );
    let output = run_client_with(&ssh, &ASK, "xyz\n", &["--transport", "stdio"]);
    assert_session(&output);
}

#[test]
fn a_blocked_udp_port_falls_back_to_tcp() {
    let marker = fake_ssh("").with_file_name("forwarded");
    let ssh = transport_ssh("", &note_forwarding(&marker));
    // nothing answers on this address (reserved for documentation)
    let started = Instant::now();
    let output = run_client_with(&ssh, &ASK, "xyz\n", &["--udp-host", "192.0.2.1"]);
    assert_session(&output);
    assert!(marker.exists(), "TCP was not used");
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn forbidden_forwarding_falls_back_to_the_ssh_channel() {
    // the host allows no forwarding, as `AllowTcpForwarding no` does
    let ssh = transport_ssh(
        "",
        "if [ -n \"$forward\" ]; then echo 'channel 0: open failed: administratively prohibited' >&2; exit 255; fi",
    );
    let output = run_client_with(&ssh, &ASK, "xyz\n", &NO_UDP);
    assert_session(&output);
}

/// An `extra` for [`transport_ssh`] that notes every use of `-W` in a file.
fn note_forwarding(marker: &Path) -> String {
    format!(
        "if [ -n \"$forward\" ]; then echo forwarded >> {}; fi",
        marker.display()
    )
}

#[test]
fn udp_is_used_when_it_works_and_a_jump_host_leaves_it_out() {
    for (config, forwarded) in [("", false), ("proxyjump bastion\n", true)] {
        let marker = fake_ssh("").with_file_name("forwarded");
        let ssh = transport_ssh(config, &note_forwarding(&marker));
        let output = run_client(&ssh, &ASK, "xyz\n");
        assert_session(&output);
        assert_eq!(marker.exists(), forwarded, "config {config:?}");
    }
}

#[test]
fn a_chosen_transport_that_the_server_does_not_offer_is_an_error() {
    let ssh = transport_ssh("", "");
    let output = run_client_with(
        &ssh,
        &ASK,
        "",
        &["--transport", "tcp", "--server-arg", "--no-tcp"],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr:?}");
    assert!(stderr.contains("no TCP port"), "{stderr:?}");
}

#[test]
fn the_port_of_the_destination_goes_to_ssh_and_not_into_the_host_name() {
    let log = fake_ssh("").with_file_name("args.log");
    let script = format!(
        "#!/bin/sh\necho \"$*\" >> {}\n{}",
        log.display(),
        &FAKE_SSH["#!/bin/sh\n".len()..]
    );
    let output = run_client_to(
        &fake_ssh(&script),
        &["-c", "exit 0"],
        "",
        &[],
        "ssh://tim@localhost:2222",
        Some("127.0.0.1"),
    );
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let logged = std::fs::read_to_string(&log).unwrap();
    for call in logged.lines() {
        let (options, target) = call.split_once(" -- ").unwrap();
        assert!(options.contains("-p 2222"), "{call}");
        assert!(target.starts_with("tim@localhost"), "{call}");
        assert!(!target.contains(":2222"), "{call}");
    }
}

// The destination may be a name from the `ssh` configuration that DNS does not know. UDP goes to
// the `HostName` that `ssh -G` reports.
#[test]
fn udp_goes_to_the_host_name_of_the_ssh_configuration() {
    let marker = fake_ssh("").with_file_name("forwarded");
    let ssh = transport_ssh("hostname 127.0.0.1\n", &note_forwarding(&marker));
    let output = run_client_to(&ssh, &ASK, "xyz\n", &[], "tim@no-such-alias.invalid", None);
    assert_session(&output);
    assert!(!marker.exists(), "UDP was not used");
}

#[test]
fn a_named_session_is_started_and_joined_again_by_its_name() {
    let runtime = short_dir();
    let ssh = fake_ssh(FAKE_SSH);
    let shell = [
        "--server-arg",
        "--shell",
        "--server-arg",
        "/bin/sh",
        "--server-arg",
        "--shell-arg",
        "--server-arg",
        "-c",
        "--server-arg",
        "--shell-arg",
        "--server-arg",
        "stty -echo; n=0; while read l; do n=$((n+1)); echo \"got:$l:$n\"; done",
    ];
    let mut words = vec!["--name", "work"];
    words.extend(shell);
    words.push("user@localhost");

    // the first call starts the session, the shell counts its lines
    let first = ssj_typing(
        &runtime,
        &ssh,
        &words,
        &[("one\n", "got:one:1"), ("~.", "")],
    );
    assert!(
        text(&first.stdout).contains("got:one:1"),
        "{} / {}",
        text(&first.stdout),
        text(&first.stderr)
    );
    assert_eq!(first.status.code(), Some(0), "{}", text(&first.stderr));

    let listing = text(&ssj(&runtime, &ssh, &["list", "user@localhost"], "").stdout);
    assert!(
        listing.contains("detached") && listing.contains("work"),
        "{listing}"
    );

    // the same call finds the session and the counter goes on
    let second = ssj_typing(
        &runtime,
        &ssh,
        &["--name", "work", "user@localhost"],
        &[("two\n", "got:two:2"), ("~.", "")],
    );
    assert!(
        text(&second.stdout).contains("got:two:2"),
        "{} / {}",
        text(&second.stdout),
        text(&second.stderr)
    );
    assert_eq!(second.status.code(), Some(0), "{}", text(&second.stderr));
    assert_eq!(
        text(&ssj(&runtime, &ssh, &["list", "user@localhost"], "").stdout)
            .lines()
            .filter(|l| l.contains("work"))
            .count(),
        1
    );

    // the name works for attach and kill as well
    let third = ssj_typing(
        &runtime,
        &ssh,
        &["attach", "work", "user@localhost"],
        &[("three\n", "got:three:3"), ("~.", "")],
    );
    assert!(
        text(&third.stdout).contains("got:three:3"),
        "{}",
        text(&third.stderr)
    );
    let killed = ssj(&runtime, &ssh, &["kill", "work", "user@localhost"], "");
    assert!(killed.status.success(), "{}", text(&killed.stderr));
}

#[test]
fn auto_attach_joins_the_only_session_without_a_client() {
    let runtime = short_dir();
    let ssh = fake_ssh(FAKE_SSH);
    let shell = [
        "--server-arg",
        "--shell",
        "--server-arg",
        "/bin/sh",
        "--server-arg",
        "--shell-arg",
        "--server-arg",
        "-c",
        "--server-arg",
        "--shell-arg",
        "--server-arg",
        "stty -echo; n=0; while read l; do n=$((n+1)); echo \"got:$l:$n\"; done",
    ];
    let mut words = vec!["--auto-attach"];
    words.extend(shell);
    words.push("user@localhost");
    let first = ssj_typing(
        &runtime,
        &ssh,
        &words,
        &[("one\n", "got:one:1"), ("~.", "")],
    );
    assert!(
        text(&first.stdout).contains("got:one:1"),
        "{}",
        text(&first.stderr)
    );

    let second = ssj_typing(
        &runtime,
        &ssh,
        &["--auto-attach", "user@localhost"],
        &[("two\n", "got:two:2"), ("~.", "")],
    );
    assert!(
        text(&second.stdout).contains("got:two:2"),
        "{} / {}",
        text(&second.stdout),
        text(&second.stderr)
    );
    let killed = ssj(&runtime, &ssh, &["kill", "user@localhost"], "");
    assert!(killed.status.success(), "{}", text(&killed.stderr));
}

/// Types a command one key at a time on a terminal and returns everything the terminal received.
fn typed_on_a_terminal(local_echo: &str) -> String {
    use std::io::{Read, Write};
    use std::sync::mpsc;

    let ssh = fake_ssh(FAKE_SSH);
    let port = free_port();
    let (pty, pts) = pty_process::blocking::open().unwrap();
    pty.resize(pty_process::Size::new(24, 80)).unwrap();
    let mut child = pty_process::blocking::Command::new(env!("CARGO_BIN_EXE_ssj"))
        .args(["--ssh-command", ssh.to_str().unwrap()])
        .args(["--server-path", server_binary().to_str().unwrap()])
        .args(["--udp-host", "127.0.0.1", "--local-echo", local_echo])
        .args(["--port-range", &format!("{port}:{port}")])
        .args(["--detached-timeout", "5", "--server-arg", "--max-detached"])
        .args(["--server-arg", "30", "--server-arg", "--shell"])
        .args(["--server-arg", "/bin/sh", "--server-arg", "--shell-arg"])
        .args(["--server-arg", "-i", "me@localhost"])
        .spawn(pts)
        .unwrap();
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
    let wait_for = |seen: &mut String, needle: &str| {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !seen.contains(needle) {
            let left = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(left) {
                Ok(bytes) => seen.push_str(&String::from_utf8_lossy(&bytes)),
                Err(_) => panic!("never saw {needle:?} in {seen:?}"),
            }
        }
    };
    wait_for(&mut seen, "$");
    // a first line with no prediction, so that the prompt is known, then a typed one
    for line in ["true\n", "echo do$((40+2))ne\n"] {
        for byte in line.bytes() {
            pty.write_all(&[byte]).unwrap();
            std::thread::sleep(Duration::from_millis(40));
        }
    }
    wait_for(&mut seen, "do42ne");
    pty.write_all(b"exit 0\n").unwrap();
    let start = Instant::now();
    while child.try_wait().unwrap().is_none() {
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "the client hangs"
        );
        while let Ok(bytes) = rx.try_recv() {
            seen.push_str(&String::from_utf8_lossy(&bytes));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    while let Ok(bytes) = rx.try_recv() {
        seen.push_str(&String::from_utf8_lossy(&bytes));
    }
    seen
}

#[test]
fn local_echo_shows_typed_characters_ahead_and_leaves_the_session_intact() {
    let on = typed_on_a_terminal("auto");
    // characters were drawn faint ahead of the server, and each was taken back
    assert!(on.contains("\x1b[2m"), "{on:?}");
    assert!(on.contains("\x08\x1b[K"), "{on:?}");
    let off = typed_on_a_terminal("off");
    assert!(!off.contains("\x1b[2m"), "{off:?}");
}
