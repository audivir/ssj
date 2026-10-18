//! The transports that go through `ssh` itself: TCP with TLS through `ssh -W`, and the stdin and
//! stdout of `ssj-server --stdio`.

use std::io;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use ssj_net::TlsConnector;
use ssj_proto::SessionId;
use ssj_proto::bootstrap::{MAX_OUTPUT, SYNC_HEADER, find_sync, sanitize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::time::timeout;

use crate::bootstrap::remote_command;
use crate::cli::Args;
use crate::run::{Dial, Stream, Tier};

/// Text of the error output of `ssh` that is kept for the message after a failure.
const STDERR_KEPT: usize = 2048;
/// Characters of the error output shown.
const SHOWN: usize = 120;
/// Time `ssh -G` gets.
const CONFIG_TIMEOUT: Duration = Duration::from_secs(5);

/// The `ssh` command for a connection that stays open: no escape character, no terminal, and no
/// questions, since a reconnect cannot ask for a passphrase. An agent or a shared connection
/// (`ControlMaster`) provides the authentication.
fn ssh_base(args: &Args) -> Command {
    let mut ssh = Command::new(&args.ssh_command[0]);
    ssh.args(&args.ssh_command[1..])
        .args(args.ssh_port_args())
        .args(["-e", "none", "-T", "-o", "BatchMode=yes"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    ssh
}

/// A way to reach the server, without the details.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Udp,
    Tcp,
    Stdio,
}

/// The transports to try, in order, for the choice of the user and what is known about the path.
///
/// `udp` and `tcp` tell whether the server offers the port. The `ssh` channel is always there.
/// With a jump host in the way UDP is left out, since datagrams do not travel through it. A
/// transport that the user chose and that is not available leaves the list empty, so that the
/// caller can say why.
pub fn tier_order(choice: crate::cli::Transport, jump: bool, udp: bool, tcp: bool) -> Vec<Kind> {
    use crate::cli::Transport;
    match choice {
        Transport::Udp => udp.then_some(Kind::Udp).into_iter().collect(),
        Transport::Tcp => tcp.then_some(Kind::Tcp).into_iter().collect(),
        Transport::Stdio => vec![Kind::Stdio],
        Transport::Auto => {
            let mut order = Vec::new();
            if udp && !jump {
                order.push(Kind::Udp);
            }
            if tcp {
                order.push(Kind::Tcp);
            }
            order.push(Kind::Stdio);
            order
        }
    }
}

/// What `ssh -G` says about the destination that matters here.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SshConfig {
    /// The `HostName` after the aliases of the configuration, which is where UDP goes.
    pub hostname: Option<String>,
    /// A jump host or a proxy command is in the way. UDP does not travel through one.
    pub jump: bool,
}

/// Reads the output of `ssh -G`: lines of a lowercase key and a value.
pub fn parse_ssh_config(config: &str) -> SshConfig {
    let mut out = SshConfig::default();
    for line in config.lines() {
        let mut words = line.splitn(2, ' ');
        let (Some(key), Some(value)) = (words.next(), words.next()) else {
            continue;
        };
        let value = value.trim();
        match key {
            "hostname" if out.hostname.is_none() && !value.is_empty() => {
                out.hostname = Some(value.to_owned());
            }
            "proxyjump" | "proxycommand" if !value.is_empty() && value != "none" => {
                out.jump = true;
            }
            _ => {}
        }
    }
    out
}

/// Whether the configuration of `ssh` goes through a jump host or a proxy command.
pub fn jump_in_config(config: &str) -> bool {
    parse_ssh_config(config).jump
}

/// Asks `ssh -G` about the destination. `None` if `ssh` cannot say.
pub async fn ssh_config(args: &Args) -> Option<SshConfig> {
    let mut ssh = Command::new(&args.ssh_command[0]);
    ssh.args(&args.ssh_command[1..])
        .args(args.ssh_port_args())
        .args(["-G", "--"])
        .arg(&args.destination)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let child = ssh.spawn().ok()?;
    match timeout(CONFIG_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(output)) if output.status.success() => {
            Some(parse_ssh_config(&String::from_utf8_lossy(&output.stdout)))
        }
        _ => None,
    }
}

/// Collects the error output of a process in the background, so that its pipe never fills up and
/// the text is there when the process fails.
struct Stderr(Arc<Mutex<Vec<u8>>>);

impl Stderr {
    fn collect(child: &mut Child) -> Self {
        let kept = Arc::new(Mutex::new(Vec::new()));
        if let Some(mut stderr) = child.stderr.take() {
            let kept = kept.clone();
            tokio::spawn(async move {
                let mut chunk = [0u8; 512];
                while let Ok(n) = stderr.read(&mut chunk).await {
                    if n == 0 {
                        break;
                    }
                    let mut kept = kept.lock().expect("not poisoned");
                    let room = STDERR_KEPT.saturating_sub(kept.len());
                    kept.extend_from_slice(&chunk[..n.min(room)]);
                }
            });
        }
        Self(kept)
    }

    /// The text for a message, safe to show on a terminal.
    fn text(&self) -> String {
        let kept = self.0.lock().expect("not poisoned");
        sanitize(kept.trim_ascii(), SHOWN)
            .replace('?', " ")
            .trim()
            .to_owned()
    }
}

/// What an `ssh` process shows when it fails, or the reason given.
async fn explain(stderr: &Stderr, fallback: &str) -> String {
    // the text may still be on its way from the pipe
    tokio::time::sleep(Duration::from_millis(100)).await;
    let text = stderr.text();
    if text.is_empty() {
        fallback.to_owned()
    } else {
        text
    }
}

/// Stdout and stdin of a process as one stream.
struct Pipes<R> {
    out: R,
    stdin: ChildStdin,
}

impl<R: AsyncRead + Unpin> AsyncRead for Pipes<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.out).poll_read(cx, buf)
    }
}

impl<R: Unpin> AsyncWrite for Pipes<R> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stdin).poll_write(cx, data)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stdin).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stdin).poll_shutdown(cx)
    }
}

/// TCP with TLS to the loopback port of the server, through `ssh -W`.
pub fn tcp_tier(args: &Args, connector: TlsConnector, port: u16, limit: Duration) -> Tier {
    let args = args.clone();
    let dial: Dial = Box::new(move || {
        let (args, connector) = (args.clone(), connector.clone());
        Box::pin(async move { dial_tcp(&args, &connector, port, limit).await })
    });
    Tier::Stream { name: "tcp", dial }
}

async fn dial_tcp(
    args: &Args,
    connector: &TlsConnector,
    port: u16,
    limit: Duration,
) -> Result<Stream, String> {
    if args.destination.starts_with('-') {
        return Err("the destination must not start with '-'".to_owned());
    }
    let mut ssh = ssh_base(args);
    ssh.args(["-W", &format!("127.0.0.1:{port}"), "--"])
        .arg(&args.destination);
    let mut child = ssh
        .spawn()
        .map_err(|e| format!("cannot run {}: {e}", args.ssh_command[0]))?;
    let stderr = Stderr::collect(&mut child);
    let pipes = Pipes {
        out: child.stdout.take().expect("piped"),
        stdin: child.stdin.take().expect("piped"),
    };
    // the handshake fails at once when `ssh` ends because forwarding is not allowed
    match timeout(limit, connector.connect(ssj_net::server_name(), pipes)).await {
        Ok(Ok(tls)) => {
            let (reader, writer) = tokio::io::split(tls);
            Ok(Stream {
                reader: Box::new(reader),
                writer: Box::new(writer),
                guard: Some(Box::new(child)),
            })
        }
        Ok(Err(err)) => Err(explain(&stderr, &err.to_string()).await),
        Err(_) => Err("no answer".to_owned()),
    }
}

/// The stream of `ssj-server --stdio` on the `ssh` channel.
pub fn stdio_tier(args: &Args, id: &SessionId, limit: Duration) -> Tier {
    let (args, id) = (args.clone(), id.clone());
    let dial: Dial = Box::new(move || {
        let (args, id) = (args.clone(), id.clone());
        Box::pin(async move { dial_stdio(&args, &id, limit).await })
    });
    Tier::Stream {
        name: "stdio",
        dial,
    }
}

async fn dial_stdio(args: &Args, id: &SessionId, limit: Duration) -> Result<Stream, String> {
    if args.destination.starts_with('-') {
        return Err("the destination must not start with '-'".to_owned());
    }
    let command = remote_command(args, &["--stdio".to_owned(), id.as_str().to_owned()]);
    let mut ssh = ssh_base(args);
    ssh.arg("--").arg(&args.destination).arg(command);
    let mut child = ssh
        .spawn()
        .map_err(|e| format!("cannot run {}: {e}", args.ssh_command[0]))?;
    let stderr = Stderr::collect(&mut child);
    let mut stdout: ChildStdout = child.stdout.take().expect("piped");
    let stdin = child.stdin.take().expect("piped");

    // shell startup files may print text first, so the stream starts at the sync header
    let found = timeout(limit, async {
        let mut seen = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            if let Some(end) = find_sync(&seen) {
                return Ok(seen.split_off(end));
            }
            if seen.len() > MAX_OUTPUT {
                return Err("the host sent no stream".to_owned());
            }
            match stdout.read(&mut chunk).await {
                Ok(0) | Err(_) => return Err(String::new()),
                Ok(n) => seen.extend_from_slice(&chunk[..n]),
            }
        }
    })
    .await;
    let rest = match found {
        Ok(Ok(rest)) => rest,
        Ok(Err(reason)) => {
            let reason = if reason.is_empty() {
                "the connection ended".to_owned()
            } else {
                reason
            };
            return Err(explain(&stderr, &reason).await);
        }
        Err(_) => return Err("no answer".to_owned()),
    };
    let _ = SYNC_HEADER;
    let reader = std::io::Cursor::new(rest).chain(stdout);
    Ok(Stream {
        reader: Box::new(reader),
        writer: Box::new(stdin),
        guard: Some(Box::new(child)),
    })
}
