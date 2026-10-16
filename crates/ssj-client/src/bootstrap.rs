//! Starting the server through `ssh`.

use std::process::Stdio;
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

/// The command line that the remote shell runs.
pub fn remote_command(args: &Args) -> String {
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
    for arg in &args.server_args {
        command.push(' ');
        command.push_str(&shell_quote(arg));
    }
    command
}

/// Runs the bootstrap and returns the connection data of the server.
pub async fn start_server(args: &Args, client_fingerprint: &[u8; 32]) -> Result<Connect, String> {
    if args.destination.starts_with('-') {
        return Err("the destination must not start with '-'".to_owned());
    }
    let mut command = Command::new(&args.ssh_command[0]);
    // `-T`: no terminal, so that the output is not altered. `--`: no option injection.
    command
        .args(&args.ssh_command[1..])
        .args(["-T", "--"])
        .arg(&args.destination)
        .arg(remote_command(args))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|e| format!("cannot run {}: {e}", args.ssh_command[0]))?;

    let mut stdin = child.stdin.take().expect("piped");
    let pin = format_pin(client_fingerprint);
    // the server may fail before it reads the pin, which shows up in its error output below
    let _ = stdin.write_all(pin.as_bytes()).await;
    drop(stdin);

    let mut stdout = child.stdout.take().expect("piped").take(MAX_OUTPUT as u64);
    let mut stderr = child.stderr.take().expect("piped").take(4096);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let finished = timeout(BOOTSTRAP_TIMEOUT, async {
        let _ = tokio::join!(stdout.read_to_end(&mut out), stderr.read_to_end(&mut err));
        child.wait().await
    })
    .await;
    let status = match finished {
        Ok(Ok(status)) => status,
        Ok(Err(e)) => return Err(format!("ssh failed: {e}")),
        Err(_) => return Err("the bootstrap timed out".to_owned()),
    };

    match parse_connect(&out) {
        Ok(Some(connect)) if status.success() => Ok(connect),
        _ => Err(format!(
            "could not start the server ({status}): {}",
            sanitize(&err, SHOWN_CHARS)
        )),
    }
}
