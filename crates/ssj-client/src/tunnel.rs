//! The transports that go through `ssh` itself: the control socket of the session and TCP with
//! TLS, both through `ssh -W`, and the stdin and stdout of `ssj-server --stdio`.

use std::io;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use ssj_net::TlsConnector;
use ssj_proto::SessionId;
use ssj_proto::bootstrap::{MAX_OUTPUT, SYNC_HEADER, find_sync, sanitize};
use ssj_proto::control::{MAX_LINE, Reply, Request, format_request, parse_reply};
use tokio::io::ReadBuf;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
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
const CONFIG_TIMEOUT: Duration = Duration::from_secs(2);

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
    Socket,
    Tcp,
    Stdio,
}

/// The transports to try, in order, for the choice of the user and what is known about the path.
///
/// `offered` holds what the server offers of UDP, the socket, and TCP, in that order. The `ssh`
/// channel is always there. With a jump host in the way UDP is left out, since datagrams do not
/// travel through it. A transport that the user chose and that is not available leaves the list
/// empty, so that the caller can say why.
pub fn tier_order(choice: crate::cli::Transport, jump: bool, offered: [bool; 3]) -> Vec<Kind> {
    use crate::cli::Transport;
    let [udp, socket, tcp] = offered;
    let only = |kind, offered: bool| offered.then_some(kind).into_iter().collect();
    match choice {
        Transport::Udp => only(Kind::Udp, udp),
        Transport::Socket => only(Kind::Socket, socket),
        Transport::Tcp => only(Kind::Tcp, tcp),
        Transport::Stdio => vec![Kind::Stdio],
        Transport::Auto => [
            (Kind::Udp, udp && !jump),
            (Kind::Socket, socket),
            (Kind::Tcp, tcp),
            (Kind::Stdio, true),
        ]
        .into_iter()
        .filter_map(|(kind, offered)| offered.then_some(kind))
        .collect(),
    }
}

/// What `ssh -G` says about the destination that matters here.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SshConfig {
    /// The `HostName` after the aliases of the configuration, which is where UDP goes.
    pub hostname: Option<String>,
    /// A jump host or a proxy command is in the way. UDP does not travel through one.
    pub jump: bool,
    /// The patterns of `SendEnv`: `ssh` sends these variables to the host, and `ssj` sends them as
    /// well. A pattern that `ssj` would not take is left out.
    pub send_env: Vec<String>,
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
            "sendenv" if ssj_proto::EnvFilter::new(vec![value.to_owned()]).is_ok() => {
                out.send_env.push(value.to_owned());
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

/// Asks `ssh -G` about the destination, once for the whole connection. `None` if `ssh` cannot say.
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
pub(crate) struct Stderr(Arc<Mutex<Vec<u8>>>, Option<tokio::task::JoinHandle<()>>);

impl Stderr {
    pub(crate) fn collect(child: &mut Child) -> Self {
        let kept = Arc::new(Mutex::new(Vec::new()));
        let mut reader = None;
        if let Some(mut stderr) = child.stderr.take() {
            let kept = kept.clone();
            reader = Some(tokio::spawn(async move {
                let mut chunk = [0u8; 512];
                while let Ok(n) = stderr.read(&mut chunk).await {
                    if n == 0 {
                        break;
                    }
                    let mut kept = kept.lock().expect("not poisoned");
                    let room = STDERR_KEPT.saturating_sub(kept.len());
                    kept.extend_from_slice(&chunk[..n.min(room)]);
                }
            }));
        }
        Self(kept, reader)
    }

    /// What was kept, once the process has closed its error output (a moment at most).
    pub(crate) async fn finished(&mut self) -> Vec<u8> {
        if let Some(reader) = self.1.take() {
            let _ = timeout(Duration::from_secs(2), reader).await;
        }
        self.0.lock().expect("not poisoned").clone()
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
    // the text may still be on its way from the pipe.
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

/// The control socket of the session, through `ssh -W`: `ssh` already authenticates and
/// encrypts, and only the user can open the socket, so the stream carries no TLS.
pub fn socket_tier(args: &Args, path: &str, limit: Duration) -> Tier {
    let (args, path) = (args.clone(), path.to_owned());
    let dial: Dial = Box::new(move || {
        let (args, path) = (args.clone(), path.clone());
        Box::pin(async move { dial_socket(&args, &path, limit).await })
    });
    Tier::Stream {
        name: "socket",
        dial,
    }
}

async fn dial_socket(args: &Args, path: &str, limit: Duration) -> Result<Stream, String> {
    if args.destination.starts_with('-') {
        return Err("the destination must not start with '-'".to_owned());
    }
    let mut ssh = ssh_base(args);
    ssh.args(["-W", path, "--"]).arg(&args.destination);
    let mut child = ssh
        .spawn()
        .map_err(|e| format!("cannot run {}: {e}", args.ssh_command[0]))?;
    let stderr = Stderr::collect(&mut child);
    let mut stdin = child.stdin.take().expect("piped");
    // the session answers the request with one line, and the stream follows it.
    let mut stdout = BufReader::new(child.stdout.take().expect("piped"));
    let answer = timeout(limit, async {
        stdin
            .write_all(format_request(&Request::Stream).as_bytes())
            .await?;
        let mut line = Vec::new();
        (&mut stdout)
            .take(MAX_LINE as u64)
            .read_until(b'\n', &mut line)
            .await?;
        Ok::<_, io::Error>(line)
    })
    .await;
    match answer {
        Ok(Ok(line)) if matches!(parse_reply(&line), Ok(Reply::Ok)) => Ok(Stream {
            reader: Box::new(stdout),
            writer: Box::new(stdin),
            guard: Some(Box::new(child)),
        }),
        // forwarding that the host does not allow ends `ssh` at once.
        Ok(_) => Err(explain(&stderr, "the session refused the stream").await),
        Err(_) => Err("no answer".to_owned()),
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
    // the handshake fails at once when `ssh` ends because forwarding is not allowed.
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

/// Reads `stdout` until the sync header; returns the bytes after it. `seen` holds what was read
/// already. An empty error means that the output ended.
async fn read_until_sync(
    stdout: &mut ChildStdout,
    mut seen: Vec<u8>,
    limit: Duration,
) -> Result<Vec<u8>, String> {
    timeout(limit, async {
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
    .await
    .unwrap_or_else(|_| Err("no answer".to_owned()))
}

/// The `ssh` that started the server, kept open after it printed the connect line: the server
/// process waits on it for [`ssj_proto::bootstrap::STREAM_LINE`] and then carries the session
/// like `--stdio`, so that a transport other than QUIC does not need another login.
pub struct Held {
    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
    /// What came after the connect line, if anything.
    seen: Vec<u8>,
    stderr: Stderr,
}

impl Held {
    pub(crate) fn new(
        child: Child,
        stdin: ChildStdin,
        stdout: ChildStdout,
        seen: Vec<u8>,
        stderr: Stderr,
    ) -> Self {
        Self {
            child,
            stdin,
            stdout,
            seen,
            stderr,
        }
    }

    /// Asks the server process for the stream, and returns it once the stream begins.
    async fn open(self, limit: Duration) -> Result<Stream, String> {
        let Self {
            child,
            mut stdin,
            mut stdout,
            seen,
            stderr,
        } = self;
        if stdin
            .write_all(ssj_proto::bootstrap::STREAM_LINE.as_bytes())
            .await
            .and(stdin.flush().await)
            .is_err()
        {
            return Err("the first connection is closed".to_owned());
        }
        match read_until_sync(&mut stdout, seen, limit).await {
            Ok(rest) => Ok(Stream {
                reader: Box::new(std::io::Cursor::new(rest).chain(stdout)),
                writer: Box::new(stdin),
                guard: Some(Box::new(child)),
            }),
            Err(reason) if reason.is_empty() => {
                Err(explain(&stderr, "the first connection is closed").await)
            }
            Err(reason) => Err(reason),
        }
    }
}

/// Lets the first stream of `tier` run over the kept `ssh` instead of a new login. The stream
/// after it, and the stream when the kept `ssh` has ended (a server that does not hold it, or
/// a wait that was too long), is made by the dial of the tier as before.
pub fn with_held(tier: Tier, held: Held, limit: Duration) -> Tier {
    let Tier::Stream { name, mut dial } = tier else {
        return tier;
    };
    let mut held = Some(held);
    let dial: Dial = Box::new(move || {
        let fallback = dial();
        match held.take() {
            Some(held) => Box::pin(async move {
                match held.open(limit).await {
                    Ok(stream) => {
                        crate::vlog!(1, "the stream runs over the first connection");
                        Ok(stream)
                    }
                    Err(reason) => {
                        crate::vlog!(
                            1,
                            "the first connection cannot carry it ({reason}): a new login"
                        );
                        fallback.await
                    }
                }
            }),
            None => fallback,
        }
    });
    Tier::Stream { name, dial }
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

    // shell startup files may print text first, so the stream starts at the sync header.
    let rest = match read_until_sync(&mut stdout, Vec::new(), limit).await {
        Ok(rest) => rest,
        Err(reason) if reason.is_empty() => {
            return Err(explain(&stderr, "the connection ended").await);
        }
        Err(reason) => return Err(reason),
    };
    let _ = SYNC_HEADER;
    let reader = std::io::Cursor::new(rest).chain(stdout);
    Ok(Stream {
        reader: Box::new(reader),
        writer: Box::new(stdin),
        guard: Some(Box::new(child)),
    })
}
