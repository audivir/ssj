use std::ffi::OsString;
use std::time::Duration;

use ssj_server::backend::{Backend, Direct, SessionId, Shell, ShellWriter, SpawnSpec, WindowSize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::time::timeout;

fn size(rows: u16, cols: u16) -> WindowSize {
    WindowSize::new(rows, cols).unwrap()
}

fn spec(script: &str, size: WindowSize) -> SpawnSpec {
    SpawnSpec {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), script.into()],
        env: vec![(OsString::from("PATH"), OsString::from("/usr/bin:/bin"))],
        cwd: None,
        size,
    }
}

fn id() -> SessionId {
    SessionId::parse("abcdef012345").unwrap()
}

/// Reads until the shell closes its side. Linux reports that as EIO and macOS as end of file.
async fn read_all(reader: &mut (impl AsyncRead + Unpin)) -> String {
    let mut out = Vec::new();
    let mut buf = [0u8; 1024];
    loop {
        match timeout(Duration::from_secs(10), reader.read(&mut buf))
            .await
            .unwrap()
        {
            Ok(0) | Err(_) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[tokio::test]
async fn output_and_exit_status_reach_the_caller() {
    let shell = Direct
        .spawn(&id(), spec("printf hello; exit 3", size(24, 80)))
        .await
        .unwrap();
    let mut parts = shell.split();

    assert!(read_all(&mut parts.reader).await.contains("hello"));
    assert_eq!(parts.exit.await.unwrap().code(), Some(3));
}

#[tokio::test]
async fn shell_sees_initial_and_changed_window_size() {
    let shell = Direct
        .spawn(
            &id(),
            spec("stty size; read line; stty size", size(30, 100)),
        )
        .await
        .unwrap();
    let mut parts = shell.split();

    // resizing before the first output would race with the first `stty size`
    let mut seen = String::new();
    let mut buf = [0u8; 1024];
    while !seen.contains("30 100") {
        let n = timeout(Duration::from_secs(10), parts.reader.read(&mut buf))
            .await
            .unwrap()
            .unwrap();
        seen.push_str(&String::from_utf8_lossy(&buf[..n]));
    }
    parts.writer.resize(size(40, 120)).unwrap();
    parts.writer.write_all(b"\n").await.unwrap();

    assert!(read_all(&mut parts.reader).await.contains("40 120"));
}

#[tokio::test]
async fn shell_gets_only_the_environment_of_the_spec() {
    let shell = Direct
        .spawn(
            &id(),
            SpawnSpec {
                env: vec![("ONLY".into(), "this".into())],
                ..spec("/usr/bin/env", size(24, 80))
            },
        )
        .await
        .unwrap();
    let mut parts = shell.split();

    let output = read_all(&mut parts.reader).await;

    assert!(output.contains("ONLY=this"));
    // the test process itself has these
    assert!(!output.contains("HOME="), "{output}");
    assert!(!output.contains("CARGO"), "{output}");
}
