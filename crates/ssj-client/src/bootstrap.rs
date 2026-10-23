//! Running the server through `ssh`.

use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use ssj_proto::bootstrap::{
    CHOICE_EXIT_CODE, Connect, MAX_OUTPUT, NO_START_LINE, Prestart, format_pin, format_start,
    parse_connect, sanitize,
};
use ssj_proto::control::{Reply, parse_reply};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::time::timeout;

use crate::cli::{Args, ServerPath};
use crate::tunnel::{Held, Stderr};
use crate::vlog;

/// Time for the whole bootstrap, which includes typing a passphrase.
const BOOTSTRAP_TIMEOUT: Duration = Duration::from_secs(180);
/// Text of the remote side shown after a failure.
const SHOWN_CHARS: usize = 400;

/// Quotes `text` for a POSIX shell.
pub fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// Quotes a path on the host. A leading `~/` stays the home directory of the host: a shell expands
/// a tilde only when it is not quoted.
fn quote_path(path: &str) -> String {
    match path.strip_prefix("~/") {
        Some(rest) => format!("\"$HOME\"/{}", shell_quote(rest)),
        None => shell_quote(path),
    }
}

/// Text that the wrapper prints, and the exit code it uses, when the server file is missing.
pub const MISSING_MARKER: &str = "SSJ MISSING";
/// Exit code of the wrapper when the server file is missing.
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
            let mut command = quote_path(path);
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

/// Starts `command` on the host through `ssh`, with all three pipes.
fn spawn_remote(
    args: &Args,
    command: &str,
    stdin_len: Option<usize>,
) -> Result<tokio::process::Child, String> {
    if args.destination.starts_with('-') {
        return Err("the destination must not start with '-'".to_owned());
    }
    let mut ssh = Command::new(&args.ssh_command[0]);
    vlog!(
        2,
        "running: {} -e none -T -- {} <command of {} characters>, {}",
        args.ssh_command
            .iter()
            .cloned()
            .chain(args.ssh_port_args())
            .collect::<Vec<_>>()
            .join(" "),
        args.destination,
        command.len(),
        stdin_len.map_or_else(
            || "input follows".to_owned(),
            |n| format!("{n} bytes on stdin")
        )
    );
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
    ssh.spawn()
        .map_err(|e| format!("cannot run {}: {e}", args.ssh_command[0]))
}

/// Runs `command` through `ssh` with `stdin` as its input and collects the output.
/// Most error output of `ssh` that is read.
const MAX_ERROR_OUTPUT: usize = 4096;

pub async fn run_remote_command(
    args: &Args,
    command: &str,
    stdin: Vec<u8>,
) -> Result<Remote, String> {
    let began = std::time::Instant::now();
    let mut child = spawn_remote(args, command, Some(stdin.len()))?;

    let mut input = child.stdin.take().expect("piped");
    let mut stdout = child.stdout.take().expect("piped").take(MAX_OUTPUT as u64);
    let mut stderr = child
        .stderr
        .take()
        .expect("piped")
        .take(MAX_ERROR_OUTPUT as u64);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    // a process that has more to say than is read blocks on its full pipe and never ends, so
    // the end of the reading that is cut short stops it.
    let too_much = tokio::sync::Notify::new();
    let finished = timeout(BOOTSTRAP_TIMEOUT, async {
        // writing and reading run side by side, so that a large upload cannot block on output.
        let write = async move {
            // the remote side may stop reading early, which shows up in its error output.
            let _ = input.write_all(&stdin).await;
            drop(input);
        };
        let read_out = async {
            let _ = stdout.read_to_end(&mut out).await;
            if out.len() >= MAX_OUTPUT {
                too_much.notify_one();
            }
        };
        let read_err = async {
            let _ = stderr.read_to_end(&mut err).await;
            if err.len() >= MAX_ERROR_OUTPUT {
                too_much.notify_one();
            }
        };
        let reading = async { tokio::join!(write, read_out, read_err) };
        tokio::pin!(reading);
        tokio::select! {
            _ = &mut reading => {}
            () = too_much.notified() => {
                let _ = child.start_kill();
                (&mut reading).await;
            }
        }
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

/// Reads the answer of a finished server process: the connection data, or why there is none.
fn interpret(remote: Remote) -> Result<Connect, StartError> {
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

/// An `ssh` that runs the server and waits for its input, which [`start_server`] writes.
pub struct Spawned {
    child: tokio::process::Child,
    began: std::time::Instant,
}

/// Starts the `ssh` of [`start_server`] before the input is known: the login takes longer than
/// what the input waits for (`ssh -G`, the size of the window), and the server reads the input
/// only after the login.
pub fn spawn_server(args: &Args, extra: &[String]) -> Result<Spawned, String> {
    let command = remote_command(args, extra);
    let began = std::time::Instant::now();
    let child = spawn_remote(args, &command, None)?;
    Ok(Spawned { child, began })
}

/// Starts the server, or attaches to one that runs, and returns its connection data.
///
/// The `ssh` is not closed after the connect line: the server process waits on it for a request
/// to carry the session itself, and the `Held` that is returned lets a transport other than QUIC
/// use it instead of a second login. A server that does not wait ends the `ssh` at once, and the
/// `Held` then fails when it is used.
///
/// `extra` holds the arguments for the attach case; the client fingerprint goes in on stdin. With
/// `prestart` a fresh session starts its shell at once, while the client is still connecting.
pub async fn start_server(
    args: &Args,
    client_fingerprint: &[u8; 32],
    extra: &[String],
    prestart: Option<&Prestart>,
    spawned: Option<Spawned>,
) -> Result<(Connect, Option<Held>), StartError> {
    let mut input = format_pin(client_fingerprint);
    // the server waits a moment for a second line, which an open input cannot end by closing.
    input.push_str(
        &prestart
            .and_then(format_start)
            .unwrap_or_else(|| NO_START_LINE.to_owned()),
    );
    let Spawned { mut child, began } = match spawned {
        Some(spawned) => spawned,
        None => spawn_server(args, extra).map_err(StartError::Failed)?,
    };
    let mut stdin = child.stdin.take().expect("piped");
    let mut stdout = child.stdout.take().expect("piped");
    let mut stderr = Stderr::collect(&mut child);

    let read = timeout(BOOTSTRAP_TIMEOUT, async {
        // the remote side may stop reading early, which shows up in its error output.
        let _ = stdin.write_all(input.as_bytes()).await;
        let _ = stdin.flush().await;
        let mut out = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            if let Ok(Some(connect)) = parse_connect(&out) {
                return (Some(connect), out);
            }
            if out.len() >= MAX_OUTPUT {
                return (None, out);
            }
            match stdout.read(&mut chunk).await {
                Ok(0) | Err(_) => return (None, out),
                Ok(n) => out.extend_from_slice(&chunk[..n]),
            }
        }
    })
    .await;
    match read {
        Ok((Some(connect), out)) => {
            vlog!(
                2,
                "the server answered after {:?}: keeping the ssh open for a stream",
                began.elapsed()
            );
            let after = out.iter().rposition(|b| *b == b'\n').map_or(0, |at| at + 1);
            let held = Held::new(child, stdin, stdout, out[after..].to_vec(), stderr);
            Ok((connect, Some(held)))
        }
        Ok((None, mut rest)) => {
            // the process ended, or said too much: collect what is left, as for a plain command.
            drop(stdin);
            let _ = (&mut stdout)
                .take(MAX_OUTPUT.saturating_sub(rest.len()) as u64)
                .read_to_end(&mut rest)
                .await;
            let status = match timeout(Duration::from_secs(5), child.wait()).await {
                Ok(Ok(status)) => status,
                Ok(Err(e)) => return Err(StartError::Failed(format!("ssh failed: {e}"))),
                Err(_) => {
                    let _ = child.start_kill();
                    return Err(StartError::Failed(
                        "the command on the host did not end".to_owned(),
                    ));
                }
            };
            vlog!(2, "ssh ended after {:?}: {status:?}", began.elapsed());
            interpret(Remote {
                stdout: rest,
                stderr: stderr.finished().await,
                status,
            })
            .map(|connect| (connect, None))
        }
        Err(_) => Err(StartError::Failed(
            "the command on the host timed out".to_owned(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::quote_path;

    fn echoed(path: &str) -> String {
        let out = Command::new("sh")
            .args(["-c", &format!("printf %s {}", quote_path(path))])
            .env("HOME", "/home/someone")
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap()
    }

    #[test]
    fn a_path_in_the_home_directory_of_the_host_is_expanded_there() {
        assert_eq!(echoed("~/bin/ssj-server"), "/home/someone/bin/ssj-server");
        assert_eq!(echoed("~/a b/it's"), "/home/someone/a b/it's");
    }

    #[test]
    fn any_other_path_is_taken_as_it_is() {
        assert_eq!(echoed("/opt/ssj server"), "/opt/ssj server");
        assert_eq!(echoed("~other/x"), "~other/x");
        assert_eq!(echoed("$HOME/x"), "$HOME/x");
    }
}
