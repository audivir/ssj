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
