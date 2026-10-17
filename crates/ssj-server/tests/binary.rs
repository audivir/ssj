mod common;

use std::time::Duration;

use common::*;
use ssj_net::Identity;
use ssj_proto::bootstrap::format_pin;
use ssj_proto::{ClientFrame, Env, ExitStatus, Hello, WindowSize};
use tokio::time::timeout;

fn size() -> WindowSize {
    WindowSize::new(24, 80).unwrap()
}

#[tokio::test]
async fn bootstrap_prints_one_line_releases_the_pipe_and_leaves_a_working_daemon() {
    let runtime = runtime_dir();
    let client = Identity::generate();
    let launched = launch(
        &runtime,
        &client,
        free_range(),
        "read x; echo got:$x; exit 3",
    );

    // exactly one line, so that nothing else can be mistaken for it
    assert_eq!(launched.stdout.iter().filter(|b| **b == b'\n').count(), 1);
    assert!(
        launched.closed_after < Duration::from_secs(5),
        "{:?}",
        launched.closed_after
    );

    // the parent is gone and its pipes are closed, yet the daemon serves the session
    let (mut tx, mut rx, _conn) = open(&client, &launched.connect).await.unwrap();
    send(
        &mut tx,
        ClientFrame::Hello(Hello::Start {
            size: size(),
            env: Env::default(),
        }),
    )
    .await;
    send(&mut tx, ClientFrame::Input(b"binary\n".to_vec())).await;
    let (output, exit) = read_until(&mut rx, |_| false).await;

    assert!(output.contains("got:binary"), "{output}");
    assert_eq!(exit, Some(ExitStatus::Code(3)));

    // after the client confirms, the daemon ends and its port is closed
    send(&mut tx, ClientFrame::Close).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!reachable(&client, &launched.connect).await);
}

#[tokio::test]
async fn detach_closes_the_endpoint_of_the_daemon() {
    let runtime = runtime_dir();
    let client = Identity::generate();
    let launched = launch(&runtime, &client, free_range(), "cat");
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
    let mut chunk = [0u8; 64];
    timeout(Duration::from_secs(10), rx.read(&mut chunk))
        .await
        .unwrap()
        .unwrap();

    send(&mut tx, ClientFrame::Detach).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    assert!(!reachable(&client, &launched.connect).await);
}

#[test]
fn bad_pin_line_is_refused_without_output() {
    let runtime = runtime_dir();
    for stdin in ["garbage\n", "", "SSJ PIN abc\n"] {
        let (status, stdout, stderr, _) = run_bootstrap(&runtime, stdin, free_range(), "cat");
        assert_eq!(status.code(), Some(2), "{stdin:?}");
        assert!(stdout.is_empty(), "{stdin:?}");
        assert!(!stderr.is_empty(), "{stdin:?}");
    }
}

#[test]
fn single_port_range_is_used_and_a_busy_range_is_reported() {
    let runtime = runtime_dir();
    let client = Identity::generate();
    let range = free_range();
    let launched = launch(&runtime, &client, range, "cat");
    assert_eq!(launched.connect.port, range.0);

    // the port is taken by the daemon now, so a second server finds nothing
    let pin = format_pin(&client.fingerprint());
    let (status, stdout, stderr, _) = run_bootstrap(&runtime, &pin, range, "cat");
    assert_eq!(status.code(), Some(1));
    assert!(stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&stderr).contains("no free UDP port"),
        "{}",
        String::from_utf8_lossy(&stderr)
    );
}

#[test]
fn config_file_is_validated_and_the_server_refuses_a_bad_one() {
    use std::os::unix::fs::PermissionsExt;

    let runtime = runtime_dir();
    let config_home = runtime_dir();
    std::fs::create_dir_all(config_home.join("ssj")).unwrap();
    let file = config_home.join("ssj/server.toml");
    std::fs::write(
        &file,
        "accept_env = [\"ONLY_THIS\"]\nport_range = \"1024:1024\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();

    let start = |extra: &[&str]| {
        let mut command = server_command(&runtime);
        command.env("XDG_CONFIG_HOME", &config_home);
        let port = free_port();
        command.args([
            "--shell",
            "/bin/sh",
            "--shell-arg",
            "-c",
            "--shell-arg",
            "cat",
        ]);
        command.args(["--port-range", &format!("{port}:{port}")]);
        command.args(extra);
        use std::io::Write;
        use std::process::Stdio;
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let pin = format_pin(&Identity::generate().fingerprint());
        child
            .stdin
            .take()
            .unwrap()
            .write_all(pin.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    };

    // the flag for the port range wins, so the unusable 1024 of the file is not tried
    let ok = start(&[]);
    assert!(
        ok.status.success(),
        "{}",
        String::from_utf8_lossy(&ok.stderr)
    );

    // a broken file stops the server before it does anything
    std::fs::write(&file, "accept_env = \"TERM\"\n").unwrap();
    let broken = start(&[]);
    assert_eq!(broken.status.code(), Some(2));
    assert!(broken.stdout.is_empty());
    assert!(String::from_utf8_lossy(&broken.stderr).contains("accept_env must be a list"));

    // a file that others can write is refused
    std::fs::write(&file, "").unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o666)).unwrap();
    let writable = start(&[]);
    assert_eq!(writable.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&writable.stderr).contains("can be changed by others"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accept_env_of_the_file_decides_which_client_variables_reach_the_shell() {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Stdio;

    let runtime = runtime_dir();
    let config_home = runtime_dir();
    std::fs::create_dir_all(config_home.join("ssj")).unwrap();
    let file = config_home.join("ssj/server.toml");
    std::fs::write(&file, "accept_env = [\"ONLY_THIS\"]\n").unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();

    let client = Identity::generate();
    let port = free_port();
    let mut command = server_command(&runtime);
    command
        .env("XDG_CONFIG_HOME", &config_home)
        .args([
            "--shell",
            "/bin/sh",
            "--shell-arg",
            "-c",
            "--shell-arg",
            "env",
        ])
        .args(["--port-range", &format!("{port}:{port}")])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format_pin(&client.fingerprint()).as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let connect = ssj_proto::bootstrap::parse_connect(&output.stdout)
        .unwrap()
        .unwrap();

    let (mut tx, mut rx, _conn) = open(&client, &connect).await.unwrap();
    let env = Env::new(vec![
        ("ONLY_THIS".into(), "yes".into()),
        ("TERM".into(), "xterm-x".into()),
    ])
    .unwrap();
    send(
        &mut tx,
        ClientFrame::Hello(Hello::Start { size: size(), env }),
    )
    .await;
    let (shown, _) = read_until(&mut rx, |_| false).await;

    assert!(shown.contains("ONLY_THIS=yes"), "{shown}");
    assert!(
        !shown.contains("TERM=xterm-x"),
        "the file replaces the default list: {shown}"
    );
}
