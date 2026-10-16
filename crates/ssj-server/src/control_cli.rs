//! `ssj-server --list`, `--attach`, and `--kill`: short-lived processes that talk to a running one.

use std::io::Write;
use std::process::ExitCode;
use std::time::Duration;

use ssj_proto::bootstrap::parse_pin;
use ssj_proto::control::{Reply, Request, format_reply};
use ssj_proto::{PROTOCOL, SessionId};

use crate::cli::{Args, Mode};
use crate::control_socket::{Found, ask, control_dir, find_sessions};
use crate::launch::read_line_with_timeout;

const PIN_TIMEOUT: Duration = Duration::from_secs(10);

fn fail(message: &str) -> ExitCode {
    eprintln!("ssj-server: {message}");
    ExitCode::from(1)
}

/// Picks the session an order is meant for.
///
/// Without an id, the only session of this protocol is meant. Sessions of other protocols
/// belong to other versions of the server and are never touched.
pub fn choose(sessions: Vec<Found>, id: Option<&SessionId>) -> Result<Found, String> {
    if let Some(id) = id {
        let found = sessions
            .into_iter()
            .find(|f| &f.info.id == id)
            .ok_or_else(|| format!("no running session {}", id.as_str()))?;
        return if found.info.protocol == PROTOCOL {
            Ok(found)
        } else {
            Err(format!(
                "session {} speaks protocol {}, this server speaks {PROTOCOL}",
                id.as_str(),
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
            let ids: Vec<&str> = mine.iter().map(|f| f.info.id.as_str()).collect();
            Err(format!(
                "several sessions are running ({}), name one",
                ids.join(", ")
            ))
        }
    }
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
        Mode::Kill(id) => match choose(sessions, id.as_ref()) {
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
        Mode::Attach(id) => {
            let Some(line) = read_line_with_timeout(std::io::stdin(), PIN_TIMEOUT) else {
                return fail("no pin line on stdin");
            };
            let Ok(pin) = parse_pin(&line) else {
                return fail("stdin does not hold a valid SSJ PIN line");
            };
            match choose(sessions, id.as_ref()) {
                Err(message) => fail(&message),
                Ok(found) => match ask(&found.path, &Request::Attach(pin)) {
                    Ok(Reply::Connect(connect)) => {
                        let mut out = std::io::stdout();
                        let _ = out.write_all(format_reply(&Reply::Connect(connect)).as_bytes());
                        let _ = out.flush();
                        ExitCode::SUCCESS
                    }
                    Ok(Reply::Error(text)) => fail(&text),
                    Ok(_) => fail("unexpected reply"),
                    Err(err) => fail(&err.to_string()),
                },
            }
        }
        Mode::Serve => unreachable!("handled by the caller"),
    }
}
