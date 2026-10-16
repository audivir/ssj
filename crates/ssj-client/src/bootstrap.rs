//! Running the server through `ssh`.

use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use ssj_proto::bootstrap::{Connect, MAX_OUTPUT, format_pin, parse_connect, sanitize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::time::timeout;

use crate::cli::{Args, ServerPath, default_server_file};

/// Time for the whole bootstrap, which includes typing a passphrase.
const BOOTSTRAP_TIMEOUT: Duration = Duration::from_secs(180);
/// Text of the remote side shown after a failure.
const SHOWN_CHARS: usize = 400;

/// Quotes `text` for a POSIX shell.
pub fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// The command line that the remote shell runs: the server, its arguments, and `extra`.
pub fn remote_command(args: &Args, extra: &[String]) -> String {
    let program = match &args.server_path {
        ServerPath::Default => {
            format!(
                "\"${{XDG_DATA_HOME:-$HOME/.local/share}}/ssj/bin/{}\"",
                default_server_file()
            )
        }
        ServerPath::Custom(path) => shell_quote(path),
    };
    let mut command = program;
    for arg in args.server_args.iter().chain(extra) {
        command.push(' ');
        command.push_str(&shell_quote(arg));
    }
    command
}

/// What a command on the remote host produced.
pub struct Remote {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub status: ExitStatus,
}

impl Remote {
    /// The error output, safe to show on a terminal.
    pub fn error_text(&self) -> String {
        sanitize(&self.stderr, SHOWN_CHARS)
    }
}

/// Runs the server with `extra` arguments through `ssh` and collects its output.
pub async fn run_remote(args: &Args, extra: &[String], stdin_text: &str) -> Result<Remote, String> {
    if args.destination.starts_with('-') {
        return Err("the destination must not start with '-'".to_owned());
    }
    let mut command = Command::new(&args.ssh_command[0]);
    // `-T`: no terminal, so that the output is not altered. `--`: no option injection.
    command
        .args(&args.ssh_command[1..])
        .args(["-T", "--"])
        .arg(&args.destination)
        .arg(remote_command(args, extra))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|e| format!("cannot run {}: {e}", args.ssh_command[0]))?;

    let mut stdin = child.stdin.take().expect("piped");
    // the server may fail before it reads its input, which shows up in its error output
    let _ = stdin.write_all(stdin_text.as_bytes()).await;
    drop(stdin);

    let mut stdout = child.stdout.take().expect("piped").take(MAX_OUTPUT as u64);
    let mut stderr = child.stderr.take().expect("piped").take(4096);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let finished = timeout(BOOTSTRAP_TIMEOUT, async {
        let _ = tokio::join!(stdout.read_to_end(&mut out), stderr.read_to_end(&mut err));
        child.wait().await
    })
    .await;
    match finished {
        Ok(Ok(status)) => Ok(Remote {
            stdout: out,
            stderr: err,
            status,
        }),
        Ok(Err(e)) => Err(format!("ssh failed: {e}")),
        Err(_) => Err("the command on the host timed out".to_owned()),
    }
}

/// Starts the server, or attaches to one that runs, and returns its connection data.
///
/// `extra` holds the arguments for the attach case; the client fingerprint goes in on stdin.
pub async fn start_server(
    args: &Args,
    client_fingerprint: &[u8; 32],
    extra: &[String],
) -> Result<Connect, String> {
    let remote = run_remote(args, extra, &format_pin(client_fingerprint)).await?;
    match parse_connect(&remote.stdout) {
        Ok(Some(connect)) if remote.status.success() => Ok(connect),
        _ => Err(format!(
            "could not start the server ({}): {}",
            remote.status,
            remote.error_text()
        )),
    }
}
