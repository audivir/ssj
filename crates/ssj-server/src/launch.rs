//! Start of the server: the bootstrap process that `ssh` runs, and the daemon it creates.
//!
//! The bootstrap process reads the pinned client fingerprint from stdin, starts the daemon, waits
//! for the line with port and server fingerprint, prints it, and exits. The daemon then holds no
//! file of the `ssh` channel, so `ssh` ends while the session keeps running.

use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, ExitCode, Stdio};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use ssj_proto::bootstrap::{format_connect, format_pin, parse_connect, parse_pin};
use ssj_proto::control::SessionState;
use ssj_proto::{EnvFilter, SessionId};
use tokio::net::UnixListener;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{Mutex, mpsc as tokio_mpsc};

use crate::backend::Direct;
use crate::cli::Args;
use crate::config::Config;
use crate::control_server::{Context, serve_control};
use crate::control_socket::{SocketFile, control_dir, ensure_private_dir, socket_path};
use crate::daemon;
use crate::frontend::Frontend;
use crate::session::{
    Session, SessionCommand, SessionConfig, SessionEnd, SessionEvent, SpawnTemplate,
};

/// Longest line the bootstrap accepts from stdin or from the daemon.
pub(crate) const MAX_LINE: usize = 512;
const PIN_TIMEOUT: Duration = Duration::from_secs(10);
const START_TIMEOUT: Duration = Duration::from_secs(15);
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// Time a connected client may leave a full output buffer alone before it counts as away.
const AWAY_AFTER: Duration = Duration::from_secs(10);

/// Reads one line of at most [`MAX_LINE`] bytes in a thread, so that the wait has a limit.
pub(crate) fn read_line_with_timeout(
    source: impl Read + Send + 'static,
    limit: Duration,
) -> Option<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut line = Vec::new();
        let mut reader = BufReader::new(source.take(MAX_LINE as u64));
        let result = reader.read_until(b'\n', &mut line).map(|_| line);
        let _ = tx.send(result);
    });
    rx.recv_timeout(limit).ok()?.ok()
}

fn fail(message: &str, code: u8) -> ExitCode {
    eprintln!("ssj-server: {message}");
    ExitCode::from(code)
}

/// The process started by `ssh`.
pub fn run_parent(args: &Args) -> ExitCode {
    // the daemon gets the resolved values on its command line, so both agree
    let mut args = args.clone();
    match Config::load() {
        Ok(config) => args.apply_config(&config),
        Err(message) => return fail(&message, 2),
    }
    let args = &args;
    if daemon::is_root() && !args.allow_root {
        return fail("refusing to run as root (use --allow-root to override)", 2);
    }
    let Some(line) = read_line_with_timeout(std::io::stdin(), PIN_TIMEOUT) else {
        return fail("no pin line on stdin", 2);
    };
    let Ok(pin) = parse_pin(&line) else {
        return fail("stdin does not hold a valid SSJ PIN line", 2);
    };

    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(err) => return fail(&format!("cannot find the own executable: {err}"), 1),
    };
    let child = Command::new(exe)
        .args(args.to_child_args())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(err) => return fail(&format!("cannot start the server process: {err}"), 1),
    };
    let mut stdin = child.stdin.take().expect("piped");
    if stdin.write_all(format_pin(&pin).as_bytes()).is_err() {
        return fail("the server process did not start", 1);
    }
    drop(stdin);

    let stdout = child.stdout.take().expect("piped");
    let Some(reply) = read_line_with_timeout(stdout, START_TIMEOUT) else {
        let _ = child.kill();
        return report_child_failure(&mut child);
    };
    if parse_connect(&reply).ok().flatten().is_none() {
        return report_child_failure(&mut child);
    }
    let mut out = std::io::stdout();
    if out.write_all(&reply).and_then(|_| out.flush()).is_err() {
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

fn report_child_failure(child: &mut std::process::Child) -> ExitCode {
    let mut message = Vec::new();
    if let Some(stderr) = child.stderr.take() {
        let _ = stderr.take(4096).read_to_end(&mut message);
    }
    let _ = child.wait();
    let _ = std::io::stderr().write_all(&message);
    ExitCode::from(1)
}

/// Environment of the shell before the variables of the client: the own one without leftovers
/// of the bootstrap session.
fn base_env() -> Vec<(OsString, OsString)> {
    std::env::vars_os()
        .filter(|(name, _)| {
            let name = name.to_string_lossy();
            !(name.starts_with("SSH_")
                || name.starts_with("XDG_SESSION_")
                || name.starts_with("SSJ_"))
        })
        .collect()
}

/// The daemon: runs the session until it ends.
pub fn run_child(mut args: Args) -> ExitCode {
    let config = match Config::load() {
        Ok(config) => config,
        Err(message) => return fail(&message, 2),
    };
    args.apply_config(&config);
    daemon::detach_from_session();
    if let Err(err) = daemon::disable_core_dumps() {
        eprintln!("ssj-server: cannot disable core dumps: {err}");
    }
    let Some(line) = read_line_with_timeout(std::io::stdin(), PIN_TIMEOUT) else {
        return fail("no pin line on stdin", 2);
    };
    let Ok(pin) = parse_pin(&line) else {
        return fail("stdin does not hold a valid SSJ PIN line", 2);
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => return fail(&format!("cannot start the runtime: {err}"), 1),
    };
    runtime.block_on(run_session(args, config.accept_env(), pin))
}

async fn run_session(args: Args, accept_env: EnvFilter, pin: [u8; 32]) -> ExitCode {
    let id = daemon::random_session_id()
        .ok()
        .and_then(|id| SessionId::parse(&id));
    let Some(id) = id else {
        return fail("cannot create a session id", 1);
    };
    let (conns, conns_rx) = tokio_mpsc::channel(4);
    let mut frontend = Frontend::new(args.port_range.clone(), conns, HELLO_TIMEOUT);
    let connect = match frontend.open(pin).await {
        Ok(connect) => connect,
        Err(err) => return fail(&err.to_string(), 1),
    };

    // the control socket exists before the client hears about the session
    let socket = match prepare_control_socket(&id) {
        Ok(socket) => socket,
        Err(err) => return fail(&format!("cannot create the control socket: {err}"), 1),
    };
    let (listener, _socket_file) = socket;

    let mut out = std::io::stdout();
    let _ = out.write_all(format_connect(&connect).as_bytes());
    let _ = out.flush();
    // from here on the daemon holds nothing of the `ssh` channel
    if let Err(err) = daemon::silence_stdio() {
        eprintln!("ssj-server: cannot release stdio: {err}");
    }

    let shell = args
        .shell
        .clone()
        .or_else(|| std::env::var_os("SHELL").map(Into::into))
        .unwrap_or_else(|| "/bin/sh".into());
    let shell_args = if args.shell_args.is_empty() {
        vec!["-l".into()]
    } else {
        args.shell_args.clone()
    };
    let config = SessionConfig {
        id: id.clone(),
        template: SpawnTemplate {
            program: shell,
            args: shell_args,
            env: base_env(),
            cwd: None,
        },
        accept_env,
        replay_capacity: args.replay_capacity,
        away_after: AWAY_AFTER,
        detached_timeout: args.detached_timeout,
        max_detached: args.max_detached,
        hello_timeout: HELLO_TIMEOUT,
    };

    let (events, mut events_rx) = tokio_mpsc::unbounded_channel();
    let (commands, commands_rx) = tokio_mpsc::unbounded_channel();
    let context = Arc::new(Context {
        id,
        started: Instant::now(),
        state: std::sync::Mutex::new(SessionState::Disconnected),
        frontend: Mutex::new(frontend),
        commands: commands.clone(),
    });
    tokio::spawn(serve_control(listener, context.clone()));

    let watching = context.clone();
    tokio::spawn(async move {
        while let Some(event) = events_rx.recv().await {
            match event {
                SessionEvent::Connected => watching.set_state(SessionState::Connected),
                SessionEvent::Disconnected => watching.set_state(SessionState::Disconnected),
                SessionEvent::Detached => {
                    watching.set_state(SessionState::Detached);
                    // nobody can connect anymore, and the next attach opens a new endpoint
                    watching.frontend.lock().await.close();
                }
            }
        }
    });

    // closing like a terminal: the same as `ssj kill`. SIGHUP is ignored, since the end of the
    // `ssh` channel of the bootstrap can send it.
    for kind in [SignalKind::terminate(), SignalKind::interrupt()] {
        if let Ok(mut signal) = signal(kind) {
            let commands = commands.clone();
            tokio::spawn(async move {
                while signal.recv().await.is_some() {
                    let _ = commands.send(SessionCommand::Kill);
                }
            });
        }
    }
    if let Ok(mut hangup) = signal(SignalKind::hangup()) {
        tokio::spawn(async move { while hangup.recv().await.is_some() {} });
    }

    let end = Session::new(Direct, config)
        .with_events(events)
        .with_commands(commands_rx)
        .run(conns_rx)
        .await;
    if matches!(end, SessionEnd::Killed) {
        // the exit status for a connected client is queued, and needs a moment to leave
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    context.frontend.lock().await.close();
    match end {
        SessionEnd::Failed(_) => ExitCode::from(1),
        _ => ExitCode::SUCCESS,
    }
}

/// Binds the socket in the private directory, for this user only.
fn prepare_control_socket(id: &SessionId) -> std::io::Result<(UnixListener, SocketFile)> {
    use std::os::unix::fs::PermissionsExt;
    let dir = control_dir()?;
    ensure_private_dir(&dir)?;
    let path = socket_path(&dir, id.as_str());
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    let file = SocketFile(path.clone());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    Ok((listener, file))
}
