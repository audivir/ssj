//! The commands about the sessions of a host: list, kill, detach, and the completion.

use ssj_proto::control::{Reply, SessionInfo, SessionState, parse_reply};

use super::connect::run_choosing;
use crate::bootstrap::run_remote;
use crate::cli::Args;
use crate::install::{confirm, interactive};
use crate::sessions_cache;
use crate::terminal::ask;

/// One line of the menu for a session.
fn session_line(s: &SessionInfo) -> String {
    format!(
        "{}  {:<12}  {:>4}  {}",
        s.id.as_str(),
        s.state.word(),
        format_age(s.age_secs),
        s.name.as_ref().map_or("", |name| name.as_str())
    )
}

/// What the user picked in the menu of sessions.
pub(super) enum Picked {
    Cancelled,
    /// The line "start a new session".
    New,
    Session(String),
}

/// Shows the sessions, the youngest first (most likely the one that was just left), and asks for
/// one. With `allow_new` the last line starts a new session.
pub(super) async fn pick_session(
    title: String,
    mut sessions: Vec<SessionInfo>,
    allow_new: bool,
) -> Picked {
    sessions.sort_by_key(|s| s.age_secs);
    let mut items: Vec<String> = sessions.iter().map(session_line).collect();
    if allow_new {
        items.push("start a new session".to_owned());
    }
    let picked = tokio::task::spawn_blocking(move || {
        ask(dialoguer::Select::new()
            .with_prompt(title)
            .items(&items)
            .default(0)
            .interact_opt())
    })
    .await
    .ok()
    .flatten();
    match picked {
        None => Picked::Cancelled,
        Some(at) if at < sessions.len() => Picked::Session(sessions[at].id.as_str().to_owned()),
        Some(_) => Picked::New,
    }
}

/// Text for the age of a session: `5s`, `3m`, `2h`, or `4d`.
pub fn format_age(seconds: u64) -> String {
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m", seconds / 60),
        3600..86400 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86400),
    }
}

/// One session in the output of `ssj list --json`.
#[derive(serde::Serialize)]
struct JsonSession<'a> {
    id: &'a str,
    name: Option<&'a str>,
    state: &'a str,
    age_secs: u64,
    protocol: u16,
}

/// The sessions as a JSON array, one object each.
pub fn sessions_json(infos: &[SessionInfo]) -> String {
    let sessions: Vec<_> = infos
        .iter()
        .map(|info| JsonSession {
            id: info.id.as_str(),
            name: info.name.as_ref().map(|name| name.as_str()),
            state: info.state.word(),
            age_secs: info.age_secs,
            protocol: info.protocol,
        })
        .collect();
    serde_json::to_string(&sessions).expect("plain values serialize")
}

/// Prints the cached names for tab completion and refreshes old lists in the background.
pub(super) fn complete(host: Option<&str>, connected: bool) -> u8 {
    let Some(dir) = sessions_cache::dir() else {
        return 0;
    };
    let names = if connected {
        sessions_cache::connected_names(&dir, host)
    } else {
        sessions_cache::names(&dir, host)
    };
    for name in names {
        println!("{name}");
    }
    let Ok(exe) = std::env::current_exe() else {
        return 0;
    };
    for origin in sessions_cache::stale_origins(&dir, sessions_cache::REFRESH_AFTER) {
        let target = match origin.port {
            Some(port) if origin.destination.contains(':') => {
                // an IPv6 address needs brackets in front of a port.
                let (user, host) = origin
                    .destination
                    .rsplit_once('@')
                    .map_or(("", origin.destination.as_str()), |(u, h)| (u, h));
                let user = if user.is_empty() {
                    String::new()
                } else {
                    format!("{user}@")
                };
                format!("ssh://{user}[{host}]:{port}")
            }
            Some(port) => format!("ssh://{}:{port}", origin.destination),
            None => origin.destination.clone(),
        };
        // the list writes the cache; the process is not waited for.
        let _ = std::process::Command::new(&exe)
            .args(["list", "--batch", &target])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
    0
}

pub(super) fn remember(origin: &sessions_cache::Origin, entries: &[(String, bool)]) {
    if let Some(dir) = sessions_cache::dir() {
        // a cache that cannot be written only costs the completion.
        let _ = sessions_cache::store_states(&dir, origin, entries);
    }
}

pub(super) async fn list_sessions(args: &Args) -> u8 {
    // one refresh per host at a time; a second one has nothing to add.
    let _refresh = if args.batch {
        match sessions_cache::dir()
            .and_then(|dir| sessions_cache::Refresh::acquire(&dir, &args.destination))
        {
            Some(lock) => Some(lock),
            None => return 0,
        }
    } else {
        None
    };
    let origin = sessions_cache::Origin {
        destination: args.destination.clone(),
        port: args.ssh_port,
    };
    let remote = match run_remote(args, &["--list".to_owned()], "").await {
        Ok(remote) if remote.server_missing() => {
            remember(&origin, &[]);
            if !args.batch {
                if args.json {
                    println!("[]");
                } else {
                    println!(
                        "no sessions (ssj-server is not installed on {})",
                        args.destination
                    );
                }
            }
            return 0;
        }
        Ok(remote) if remote.status.success() => remote,
        Ok(remote) => {
            eprintln!(
                "ssj: could not list the sessions ({}): {}",
                remote.status,
                remote.error_text()
            );
            return 1;
        }
        Err(message) => {
            eprintln!("ssj: {message}");
            return 1;
        }
    };
    let infos = session_infos(&remote.stdout);
    let mut names = Vec::new();
    for info in &infos {
        let connected = info.state == SessionState::Connected;
        names.extend(
            info.name
                .as_ref()
                .map(|name| (name.as_str().to_owned(), connected)),
        );
        names.push((info.id.as_str().to_owned(), connected));
    }
    remember(&origin, &names);
    if args.batch {
        return 0;
    }
    if args.json {
        println!("{}", sessions_json(&infos));
        return 0;
    }
    if infos.is_empty() {
        println!("no sessions");
    }
    // a header helps a person and breaks a script, so only a terminal gets one.
    let header = std::io::IsTerminal::is_terminal(&std::io::stdout());
    for line in list_lines(&infos, header) {
        println!("{line}");
    }
    0
}

/// The lines of `ssj list`, with a header line if `header` is set.
pub fn list_lines(infos: &[SessionInfo], header: bool) -> Vec<String> {
    let id_width = infos
        .iter()
        .map(|i| i.id.as_str().len())
        .max()
        .unwrap_or(0)
        .max(2);
    let row = |id: &str, state: &str, age: &str, protocol: &str, name: &str| {
        format!("{id:<id_width$}  {state:<12}  {age:>4}  {protocol:<10}  {name}")
    };
    let mut lines = Vec::new();
    if header && !infos.is_empty() {
        lines.push(row("ID", "STATE", "AGE", "PROTOCOL", "NAME"));
    }
    for info in infos {
        lines.push(row(
            info.id.as_str(),
            info.state.word(),
            &format_age(info.age_secs),
            &format!("protocol {}", info.protocol),
            info.name.as_ref().map_or("-", |name| name.as_str()),
        ));
    }
    lines
}

/// The sessions of the host, as `ssj list` has them.
async fn fetch_sessions(args: &Args) -> Result<Vec<SessionInfo>, String> {
    let remote = run_remote(args, &["--list".to_owned()], "").await?;
    if remote.server_missing() {
        return Ok(Vec::new());
    }
    if !remote.status.success() {
        return Err(format!(
            "could not list the sessions ({}): {}",
            remote.status,
            remote.error_text()
        ));
    }
    Ok(session_infos(&remote.stdout))
}

/// The sessions in the output of `ssj-server --list`, one line each.
pub(super) fn session_infos(stdout: &[u8]) -> Vec<SessionInfo> {
    stdout
        .split(|b| *b == b'\n')
        .filter_map(|line| match parse_reply(line) {
            Ok(Reply::Info(info)) => Some(info),
            _ => None,
        })
        .collect()
}

/// What `ssj kill HOST` does without an id: the only session is ended after a question (unless
/// `-f` is given), and several sessions go into a menu, where the choice is the consent. Returns
/// the id, or the message that ends the command.
async fn session_to_kill(args: &Args) -> Result<String, String> {
    if !interactive() {
        // the server ends the only session, and refuses when there are several, and says so when
        // there is none: only a session that would be ended without a word needs the flag
        if args.force || fetch_sessions(args).await?.is_empty() {
            return Ok(String::new());
        }
        return Err("there is no terminal to ask: name the session, or use -f".to_owned());
    }
    let mut sessions = fetch_sessions(args).await?;
    sessions.retain(|s| s.protocol == ssj_proto::PROTOCOL);
    sessions.sort_by_key(|s| s.age_secs);
    match sessions.len() {
        0 => Err("no running session".to_owned()),
        1 => {
            let s = &sessions[0];
            if !args.force {
                let what = s.name.as_ref().map_or_else(
                    || s.id.as_str().to_owned(),
                    |name| format!("{} ({})", name.as_str(), s.id.as_str()),
                );
                let question = format!(
                    "End the session {what} on {} ({}, {})?",
                    args.destination,
                    s.state.word(),
                    format_age(s.age_secs)
                );
                if !confirm(&question) {
                    return Err("not ended".to_owned());
                }
            }
            Ok(s.id.as_str().to_owned())
        }
        n => {
            let title = format!(
                "ssj: {n} sessions on {}. Which one to end?",
                args.destination
            );
            match pick_session(title, sessions, false).await {
                Picked::Session(id) => Ok(id),
                _ => Err("cancelled".to_owned()),
            }
        }
    }
}

pub(super) async fn kill_session(args: &Args, mut ids: Vec<String>) -> u8 {
    if ids.is_empty() {
        match session_to_kill(args).await {
            Ok(id) if !id.is_empty() => ids.push(id),
            Ok(_) => {}
            Err(message) => {
                eprintln!("ssj: {message}");
                return 1;
            }
        }
    }
    send_order(args, ids, &KILL).await
}

/// What `ssj kill` and `ssj detach` ask the server for, and what they say.
pub(super) struct Order {
    flag: &'static str,
    question: &'static str,
    done: &'static str,
    done_many: &'static str,
    failed: &'static str,
}

const KILL: Order = Order {
    flag: "--kill",
    question: "Which one to end?",
    done: "session ended",
    done_many: "sessions ended",
    failed: "could not end the session",
};

pub(super) const DETACH: Order = Order {
    flag: "--detach",
    question: "Which one to detach?",
    done: "client detached; the session keeps running",
    done_many: "clients detached; the sessions keep running",
    failed: "could not detach the session",
};

/// Sends the order for the sessions `ids`, or for the only one, or for one from a menu.
pub(super) async fn send_order(args: &Args, mut ids: Vec<String>, order: &Order) -> u8 {
    let count = ids.len().max(1);
    let result = if ids.len() > 1 {
        // one call to the host handles them all.
        let mut extra = vec![order.flag.to_owned()];
        extra.extend(ids);
        run_remote(args, &extra, "").await
    } else {
        run_choosing(args, order.flag, ids.pop(), order.question).await
    };
    match result {
        Ok(remote) if remote.server_missing() => {
            eprintln!(
                "ssj: no running session (ssj-server is not installed on {})",
                args.destination
            );
            1
        }
        Ok(remote) if remote.status.success() => {
            if count > 1 {
                println!("{count} {}", order.done_many);
            } else {
                println!("{}", order.done);
            }
            0
        }
        Ok(remote) => {
            eprintln!("ssj: {}: {}", order.failed, remote.error_text());
            1
        }
        Err(message) => {
            eprintln!("ssj: {message}");
            1
        }
    }
}
