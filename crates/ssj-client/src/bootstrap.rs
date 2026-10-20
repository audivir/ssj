//! Running the server through `ssh`.

use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use ssj_proto::bootstrap::{
    CHOICE_EXIT_CODE, Connect, MAX_OUTPUT, Prestart, format_pin, format_start, parse_connect,
    sanitize,
};
use ssj_proto::control::{Reply, parse_reply};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::time::timeout;

use crate::cli::{Args, ServerPath};
use crate::vlog;

/// Time for the whole bootstrap, which includes typing a passphrase.
const BOOTSTRAP_TIMEOUT: Duration = Duration::from_secs(180);
/// Text of the remote side shown after a failure.
const SHOWN_CHARS: usize = 400;

/// Quotes `text` for a POSIX shell.
pub fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// Text that the wrapper prints, and the exit code it uses, when the server file is missing.
pub const MISSING_MARKER: &str = "SSJ MISSING";
pub const MISSING_EXIT_CODE: i32 = 66;

/// Starts the server file of this protocol if it exists. The arguments follow in `"$@"`.
const WRAPPER: &str = r#"p="${XDG_DATA_HOME:-$HOME/.local/share}/ssj/bin/ssj-server-p__PROTOCOL__"
if [ -x "$p" ]; then exec "$p" "$@"; fi
echo 'SSJ MISSING'
exit 66"#;

/// A command for the remote login shell that runs `script` with POSIX `sh`, whatever that shell is.
pub fn sh_command(script: &str, args: &[String]) -> String {
    let mut command = format!("sh -c {} ssj", shell_quote(script));
    for arg in args {
        command.push(' ');
        command.push_str(&shell_quote(arg));
    }
    command
}

/// The command line that the remote shell runs: the server, its arguments, and `extra`.
pub fn remote_command(args: &Args, extra: &[String]) -> String {
    let all: Vec<String> = args.server_args.iter().chain(extra).cloned().collect();
    match &args.server_path {
        ServerPath::Default => sh_command(
            &WRAPPER.replace("__PROTOCOL__", &ssj_proto::PROTOCOL.to_string()),
            &all,
        ),
        ServerPath::Custom(path) => {
            let mut command = shell_quote(path);
            for arg in &all {
                command.push(' ');
                command.push_str(&shell_quote(arg));
            }
            command
        }
    }
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

impl Remote {
    /// Whether the wrapper reported that the server file is not installed.
    pub fn server_missing(&self) -> bool {
        self.status.code() == Some(MISSING_EXIT_CODE)
            && self
                .stdout
                .split(|b| *b == b'\n')
                .any(|line| line == MISSING_MARKER.as_bytes())
    }
}

/// Runs `command` through `ssh` with `stdin` as its input and collects the output.
pub async fn run_remote_command(
    args: &Args,
    command: &str,
    stdin: Vec<u8>,
) -> Result<Remote, String> {
    if args.destination.starts_with('-') {
        return Err("the destination must not start with '-'".to_owned());
    }
    let mut ssh = Command::new(&args.ssh_command[0]);
    vlog!(
        2,
        "running: {} -e none -T -- {} <command of {} characters>, {} bytes on stdin",
        args.ssh_command
            .iter()
            .cloned()
            .chain(args.ssh_port_args())
            .collect::<Vec<_>>()
            .join(" "),
        args.destination,
        command.len(),
        stdin.len()
    );
    let began = std::time::Instant::now();
    // `-e none`: no escape character, so that a byte of the upload cannot end the command.
    // `-T`: no terminal, so that the output is not altered. `--`: no option injection.
    ssh.args(&args.ssh_command[1..])
        .args(args.ssh_port_args())
        .args(["-e", "none", "-T", "--"])
        .arg(&args.destination)
        .arg(command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = ssh
        .spawn()
        .map_err(|e| format!("cannot run {}: {e}", args.ssh_command[0]))?;

    let mut input = child.stdin.take().expect("piped");
    let mut stdout = child.stdout.take().expect("piped").take(MAX_OUTPUT as u64);
    let mut stderr = child.stderr.take().expect("piped").take(4096);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let finished = timeout(BOOTSTRAP_TIMEOUT, async {
        // writing and reading run side by side, so that a large upload cannot block on output.
        let write = async move {
            // the remote side may stop reading early, which shows up in its error output.
            let _ = input.write_all(&stdin).await;
            drop(input);
        };
        let _ = tokio::join!(
            write,
            stdout.read_to_end(&mut out),
            stderr.read_to_end(&mut err)
        );
        child.wait().await
    })
    .await;
    vlog!(
        2,
        "ssh ended after {:?}: {:?}, {} bytes of output, {} bytes of error output",
        began.elapsed(),
        finished.as_ref().ok().and_then(|r| r.as_ref().ok()),
        out.len(),
        err.len()
    );
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

/// Runs the server with `extra` arguments through `ssh` and collects its output.
pub async fn run_remote(args: &Args, extra: &[String], stdin_text: &str) -> Result<Remote, String> {
    run_remote_command(
        args,
        &remote_command(args, extra),
        stdin_text.as_bytes().to_vec(),
    )
    .await
}

/// Why the server could not be started.
#[derive(Debug)]
pub enum StartError {
    /// The server file of this protocol is not on the host.
    Missing,
    /// Several sessions have no client and the caller asked to choose among them.
    Choose(Vec<ssj_proto::control::SessionInfo>),
    Failed(String),
}

/// Starts the server, or attaches to one that runs, and returns its connection data.
///
/// `extra` holds the arguments for the attach case; the client fingerprint goes in on stdin. With
/// `prestart` a fresh session starts its shell at once, while the client is still connecting.
pub async fn start_server(
    args: &Args,
    client_fingerprint: &[u8; 32],
    extra: &[String],
    prestart: Option<&Prestart>,
) -> Result<Connect, StartError> {
    let mut stdin = format_pin(client_fingerprint);
    if let Some(line) = prestart.and_then(format_start) {
        stdin.push_str(&line);
    }
    let remote = run_remote(args, extra, &stdin)
        .await
        .map_err(StartError::Failed)?;
    if remote.server_missing() {
        return Err(StartError::Missing);
    }
    if remote.status.code() == Some(i32::from(CHOICE_EXIT_CODE)) {
        let sessions: Vec<_> = remote
            .stdout
            .split(|b| *b == b'\n')
            .filter(|line| !line.is_empty())
            .filter_map(|line| match parse_reply(line) {
                Ok(Reply::Info(info)) => Some(info),
                _ => None,
            })
            .collect();
        if !sessions.is_empty() {
            return Err(StartError::Choose(sessions));
        }
    }
    match parse_connect(&remote.stdout) {
        Ok(Some(connect)) if remote.status.success() => Ok(connect),
        _ => Err(StartError::Failed(format!(
            "could not start the server ({}): {}",
            remote.status,
            remote.error_text()
        ))),
    }
}
