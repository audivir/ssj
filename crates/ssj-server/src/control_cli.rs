//! `ssj-server --list`, `--attach`, and `--kill`: short-lived processes that talk to a running one.

use std::io::Write;
use std::process::ExitCode;
use std::time::Duration;

use ssj_proto::PROTOCOL;
use ssj_proto::bootstrap::parse_pin;
use ssj_proto::control::{Reply, Request, format_reply};

use crate::cli::{Args, Mode};
use crate::control_socket::{Found, ask, control_dir, find_sessions};
use crate::launch::read_line_with_timeout;

const PIN_TIMEOUT: Duration = Duration::from_secs(10);

fn fail(message: &str) -> ExitCode {
    eprintln!("ssj-server: {message}");
    ExitCode::from(1)
}

/// Picks the session an order is meant for, by id or by name.
///
/// Without a selector, the only session of this protocol is meant. Sessions of other protocols
/// belong to other versions of the server and are never touched.
pub fn choose(sessions: Vec<Found>, selector: Option<&str>) -> Result<Found, String> {
    if let Some(selector) = selector {
        let found = sessions
            .into_iter()
            .find(|f| {
                f.info.id.as_str() == selector
                    || f.info.name.as_ref().is_some_and(|n| n.as_str() == selector)
            })
            .ok_or_else(|| format!("no running session {selector}"))?;
        return if found.info.protocol == PROTOCOL {
            Ok(found)
        } else {
            Err(format!(
                "session {selector} speaks protocol {}, this server speaks {PROTOCOL}",
                found.info.protocol
            ))
        };
    }
    let mut mine: Vec<Found> = sessions
        .into_iter()
        .filter(|f| f.info.protocol == PROTOCOL)
        .collect();
    match mine.len() {
        0 => Err("no running session".to_owned()),
        1 => Ok(mine.remove(0)),
        _ => {
            let shown: Vec<String> = mine
                .iter()
                .map(|f| match &f.info.name {
                    Some(name) => format!("{} ({})", f.info.id.as_str(), name.as_str()),
                    None => f.info.id.as_str().to_owned(),
                })
                .collect();
            Err(format!(
                "several sessions are running ({}), name one",
                shown.join(", ")
            ))
        }
    }
}

/// Asks the session to accept the client with the pin, and prints the line to connect.
pub fn attach_session(found: &Found, pin: [u8; 32]) -> ExitCode {
    match ask(&found.path, &Request::Attach(pin)) {
        Ok(Reply::Connect(connect)) => {
            let mut out = std::io::stdout();
            let _ = out.write_all(format_reply(&Reply::Connect(connect)).as_bytes());
            let _ = out.flush();
            ExitCode::SUCCESS
        }
        Ok(Reply::Error(text)) => fail(&text),
        Ok(_) => fail("unexpected reply"),
        Err(err) => fail(&err.to_string()),
    }
}

/// Reads the pin line of the client from stdin.
pub fn read_pin() -> Result<[u8; 32], String> {
    let line =
        read_line_with_timeout(std::io::stdin(), PIN_TIMEOUT).ok_or("no pin line on stdin")?;
    parse_pin(&line).map_err(|_| "stdin does not hold a valid SSJ PIN line".to_owned())
}

pub fn run(args: &Args) -> ExitCode {
    let sessions = match control_dir() {
        Ok(dir) => find_sessions(&dir),
        Err(err) => return fail(&err.to_string()),
    };
    match &args.mode {
        Mode::List => {
            let mut out = std::io::stdout();
            for found in sessions {
                let _ = out.write_all(format_reply(&Reply::Info(found.info)).as_bytes());
            }
            ExitCode::SUCCESS
        }
        Mode::Kill(id) => match choose(sessions, id.as_deref()) {
            Err(message) => fail(&message),
            Ok(found) => match ask(&found.path, &Request::Kill) {
                Ok(Reply::Ok) => {
                    println!("OK");
                    ExitCode::SUCCESS
                }
                Ok(Reply::Error(text)) => fail(&text),
                Ok(_) => fail("unexpected reply"),
                Err(err) => fail(&err.to_string()),
            },
        },
        Mode::Attach(selector) => {
            let pin = match read_pin() {
                Ok(pin) => pin,
                Err(message) => return fail(&message),
            };
            match choose(sessions, selector.as_deref()) {
                Err(message) => fail(&message),
                Ok(found) => attach_session(&found, pin),
            }
        }
        Mode::Stdio(id) => match choose(sessions, id.as_deref()) {
            Err(message) => fail(&message),
            Ok(found) => stdio(&found.path),
        },
        Mode::Serve | Mode::Version => unreachable!("handled by the caller"),
    }
}

/// Time the server gets to deliver the last bytes after the client closed its side.
const STDIO_DRAIN: Duration = Duration::from_secs(2);

/// Joins stdin and stdout to the session: the control socket turns into the byte stream.
fn stdio(path: &std::path::Path) -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => return fail(&format!("cannot start the runtime: {err}")),
    };
    let code = runtime.block_on(stdio_session(path));
    // the read of stdin runs on a thread that does not return on a terminal or a pipe that stays
    // open, so waiting for the runtime would hang
    runtime.shutdown_background();
    code
}

async fn stdio_session(path: &std::path::Path) -> ExitCode {
    use ssj_proto::bootstrap::SYNC_HEADER;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, copy};
    use tokio::net::UnixStream;

    let stream = match UnixStream::connect(path).await {
        Ok(stream) => stream,
        Err(err) => return fail(&err.to_string()),
    };
    let (read, mut write) = stream.into_split();
    if write
        .write_all(ssj_proto::control::format_request(&Request::Stream).as_bytes())
        .await
        .is_err()
    {
        return fail("the session does not answer");
    }
    // the answer is one line, and the stream follows it at once
    let mut from_session = BufReader::new(read);
    let mut line = Vec::new();
    let answer = tokio::time::timeout(
        crate::control_socket::IO_TIMEOUT,
        (&mut from_session)
            .take(ssj_proto::control::MAX_LINE as u64)
            .read_until(b'\n', &mut line),
    )
    .await;
    if !matches!(answer, Ok(Ok(n)) if n > 0)
        || !matches!(ssj_proto::control::parse_reply(&line), Ok(Reply::Ok))
    {
        return fail("the session refused the stream");
    }

    let mut stdout = tokio::io::stdout();
    if stdout.write_all(SYNC_HEADER).await.is_err() || stdout.flush().await.is_err() {
        return ExitCode::from(1);
    }
    let mut stdin = tokio::io::stdin();
    let mut down = Box::pin(async {
        let _ = copy(&mut from_session, &mut stdout).await;
        let _ = stdout.flush().await;
    });
    let mut up = Box::pin(async {
        let _ = copy(&mut stdin, &mut write).await;
        let _ = write.shutdown().await;
    });
    tokio::select! {
        // the session closed the stream, which is the normal end
        () = &mut down => {}
        // the client closed its side; the session may still have bytes for it
        () = &mut up => {
            let _ = tokio::time::timeout(STDIO_DRAIN, down).await;
        }
    }
    ExitCode::SUCCESS
}
