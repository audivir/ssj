//! The session of one server process: the shell, its buffered output, and the current connection.
//!
//! A single task owns all state. Connections come in as byte streams, so the logic does not
//! depend on QUIC. Reading the shell, writing to it, and talking to the client never wait for
//! each other, which is what prevents deadlocks when a large paste meets a chatty program.

use std::ffi::OsString;
use std::future::pending;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::Duration;

use ssj_net::channels::{Channels, Piece};
use ssj_proto::bootstrap::Prestart;
use ssj_proto::compress::{Compressor, inflate};
use ssj_proto::frame::{MAX_DATA_LEN, MAX_FRAME_LEN};
use ssj_proto::resume::{Action, ServerView, negotiate};
use ssj_proto::{
    ClientFrame, Env, EnvFilter, ErrorCode, ExitStatus as WireExit, Hello, ReplayBuffer,
    ServerFrame, SessionId, WindowSize, decode_client,
};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep, sleep_until, timeout, timeout_at};

use crate::backend::{Backend, Shell, ShellWriter, SpawnSpec};
use crate::control_socket::SocketFile;

/// Bytes of the shell output read at once.
const READ_CHUNK: usize = 64 * 1024;
/// Frames waiting for the writer of the connection.
const OUT_QUEUE: usize = 2;
/// Input bytes a connection may have queued for the shell.
///
/// The reader must keep parsing while the shell is busy, because acknowledgements of output are
/// in the same byte stream as the input and are what lets the shell continue. The client limits
/// its unacknowledged input to less than this, so the reader does not block on the budget.
pub const INPUT_BUDGET: usize = 2 * 1024 * 1024;

/// Output is compressed only while at least this many bytes are not acknowledged by the client
/// (waiting or on the way), which means that the link is slower than the shell.
const COMPRESS_BACKLOG: u64 = 256 * 1024;
/// Chunks shorter than this are not worth compressing.
const COMPRESS_MIN_CHUNK: usize = 1024;

/// Byte stream pair of one client connection.
pub struct Connection {
    pub reader: Box<dyn AsyncRead + Send + Unpin>,
    pub writer: Box<dyn AsyncWrite + Send + Unpin>,
}

/// Program and fixed environment of the shell. The environment of the client is added to it.
#[derive(Clone, Debug)]
pub struct SpawnTemplate {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub env: Vec<(OsString, OsString)>,
    pub cwd: Option<PathBuf>,
}

/// What the server does with output that a connected client has not taken yet.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SlowClient {
    /// The job never waits for the link. The server keeps at most `live_backlog` bytes of unsent
    /// output and skips the oldest of the rest, and the client is told how much it missed.
    #[default]
    Drop,
    /// The job waits until the client has taken the output (as with `ssh`), so nothing is lost
    /// while the client is connected, and a slow link slows the job down.
    Wait,
}

#[derive(Clone, Debug)]
pub struct SessionConfig {
    pub id: SessionId,
    pub template: SpawnTemplate,
    /// Variables of the client that reach the shell.
    pub accept_env: EnvFilter,
    /// Output bytes kept until the client acknowledges them.
    pub replay_capacity: usize,
    pub slow_client: SlowClient,
    /// Compress output for a client when the link holds it back (see [`COMPRESS_BACKLOG`]).
    pub compress: bool,
    /// With [`SlowClient::Drop`]: the most output that may wait to be sent to a connected client.
    /// What the shell wrote before the client connected (the replay) does not count.
    pub live_backlog: usize,
    /// The shell is started when the session starts, with this size, environment, and agent flag,
    /// and the first `Start` hello of a client only joins it (see [`Prestart`]).
    pub prestart: Option<Prestart>,
    /// A session that no client has joined yet ends after this long, so that a client which
    /// gave up while connecting does not leave a shell behind. It applies on top of
    /// `detached_timeout`, which only counts from the first connection.
    pub first_client_timeout: Duration,
    /// Where the socket for the agent of the client is made when the client asks for it.
    /// `None` refuses agent forwarding.
    pub agent_socket: Option<PathBuf>,
    /// How long a connected client may leave the output buffer full without acknowledging
    /// anything before the server treats it as away (a connection that died without telling).
    pub away_after: Duration,
    /// End of the session after this long without a connection. `None` waits forever.
    pub detached_timeout: Option<Duration>,
    /// Limit for the wait after a deliberate detach. `None` waits forever.
    pub max_detached: Option<Duration>,
    /// Time a new connection has to send its first frame.
    pub hello_timeout: Duration,
    /// A connection that sends nothing for this long, not even a ping, is given up. The client
    /// pings every second, so silence means that it is gone (a closed laptop, a lost phone), and
    /// QUIC alone would keep the connection for ever.
    pub lost_after: Duration,
    /// How long a finished session waits for a client that lost the connection while the exit
    /// status was on its way. The client confirms the status with a close, and without it the
    /// session cannot know that the status arrived.
    pub exit_grace: Duration,
}

/// Why the session ended.
#[derive(Debug)]
pub enum SessionEnd {
    /// The shell exited and the client has been told.
    ShellExited(ExitStatus),
    /// No client connected for the configured time.
    DetachedTimeout,
    /// The shell could not be started or failed.
    Failed(io::Error),
    /// The sender of connections was dropped.
    Closed,
    /// A command ended the session by hanging up the terminal of the shell.
    Killed,
}

/// Changes of the state of the session that the transport needs to know about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionEvent {
    /// A client completed its handshake.
    Connected,
    /// The connected client was lost and may come back.
    Disconnected,
    /// The client left on purpose. The transport closes its endpoint and drops the pinned client.
    Detached,
}

/// Orders to the running session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionCommand {
    /// Hangs up the terminal of the shell, as closing a terminal window would, and ends.
    Kill,
}

/// Manages the shell of one session across the connections of its client.
pub struct Session<B: Backend> {
    backend: B,
    config: SessionConfig,
    events: Option<mpsc::UnboundedSender<SessionEvent>>,
    commands: Option<mpsc::UnboundedReceiver<SessionCommand>>,
}

enum Control {
    Hello(Hello),
    Resize(WindowSize),
    OutputAck(u64),
    Close,
    Detach,
    Ping,
    AgentOffer,
    Agent(u32, Vec<u8>),
    AgentClose(u32),
    Lost,
    Violation(ErrorCode),
}

struct Tagged {
    generation: u64,
    control: Control,
}

/// Aborts the task when dropped, so a replaced connection stops reading at once.
struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct Active {
    generation: u64,
    out: mpsc::Sender<ServerFrame>,
    input: mpsc::UnboundedReceiver<Vec<u8>>,
    /// Bytes that may still be queued in `input`.
    budget: Arc<Semaphore>,
    hello_done: bool,
    /// A `Ping` was received and its `Pong` is not sent yet.
    pong_due: bool,
    /// Output offset up to which frames were queued for this connection.
    output_sent: u64,
    /// Input offset that was last acknowledged to this client.
    input_acked: u64,
    /// Output offset when the client joined: what lies before it is replay and is not skipped
    /// for being slow.
    replay_end: u64,
    /// Bytes of output skipped and not announced to the client yet.
    gap: u64,
    /// The channels to the agent of the client, if this connection serves it.
    agent: Option<Channels>,
    _reader: AbortOnDrop,
    writer: Option<JoinHandle<()>>,
}

impl Drop for Active {
    fn drop(&mut self) {
        // frames already queued get a few seconds to leave, then the task is stopped
        if let Some(writer) = self.writer.take() {
            tokio::spawn(async move {
                let abort = writer.abort_handle();
                if tokio::time::timeout(Duration::from_secs(5), writer)
                    .await
                    .is_err()
                {
                    abort.abort();
                }
            });
        }
    }
}

struct Running<S: Shell> {
    reader: S::Reader,
    writer: S::Writer,
    exit: Option<crate::backend::ExitFuture>,
    status: Option<ExitStatus>,
    reader_eof: bool,
    exit_queued: bool,
}

impl<B: Backend> Session<B> {
    pub fn new(backend: B, config: SessionConfig) -> Self {
        Self {
            backend,
            config,
            events: None,
            commands: None,
        }
    }

    /// Takes [`SessionCommand`]s from `commands`.
    pub fn with_commands(mut self, commands: mpsc::UnboundedReceiver<SessionCommand>) -> Self {
        self.commands = Some(commands);
        self
    }

    /// Reports [`SessionEvent`]s on `events`.
    pub fn with_events(mut self, events: mpsc::UnboundedSender<SessionEvent>) -> Self {
        self.events = Some(events);
        self
    }

    pub async fn run(self, mut conns: mpsc::Receiver<Connection>) -> SessionEnd {
        let Self {
            backend,
            config,
            events,
            mut commands,
        } = self;
        let (control_tx, mut control_rx) = mpsc::channel::<Tagged>(64);
        let mut output = ReplayBuffer::new(config.replay_capacity);
        let mut input_received = 0u64;
        let mut pending_input: Vec<u8> = Vec::new();
        let mut pending_offset = 0usize;
        let mut pending_budget: Option<Arc<Semaphore>> = None;
        let mut shell: Option<Running<B::Shell>> = None;
        let mut active: Option<Active> = None;
        let mut generation = 0u64;
        let mut detached_since = Some(Instant::now());
        // The newest size of a burst of resizes, applied when no other message is queued.
        let mut pending_resize: Option<WindowSize> = None;
        // a deliberate detach disables the timeout until the next connection
        let mut detached_on_purpose = false;
        // since when the connection is gone although the exit status was sent
        let mut exit_lost: Option<Instant> = None;
        // since when a connected client has left the output buffer full
        let mut full_since: Option<Instant> = None;
        let mut buf = vec![0u8; READ_CHUNK];
        let mut compressor = Compressor::new();
        // the socket of the forwarded agent, once the client has asked for it
        let mut agent_listener: Option<(UnixListener, SocketFile)> = None;
        let mut next_channel = 0u32;
        // whether a client ever completed its handshake
        let mut ever_connected = false;
        if let Some(start) = config.prestart.clone() {
            match start_shell(
                &backend,
                &config,
                start.size,
                &start.env,
                start.agent,
                &mut agent_listener,
            )
            .await
            {
                Ok(running) => shell = Some(running),
                Err(err) => return SessionEnd::Failed(err),
            }
        }

        loop {
            enum Event {
                Connection(Option<Connection>),
                Control(Option<Tagged>),
                Input(Option<Vec<u8>>),
                ShellOutput(io::Result<usize>),
                InputWritten(io::Result<usize>),
                Exited(io::Result<ExitStatus>),
                CanSend(Result<mpsc::OwnedPermit<ServerFrame>, mpsc::error::SendError<()>>),
                DetachedTimeout,
                ExitGrace,
                AwayTimer,
                Command(Option<SessionCommand>),
                AgentConnected(io::Result<UnixStream>),
            }

            // Backpressure holds only while somebody is there to acknowledge the output. With
            // nobody there (no connection, or one that stays silent while the buffer is full) the
            // job must go on, so the oldest output is forgotten, as a `tmux` session does.
            if config.slow_client == SlowClient::Drop
                && let Some(a) = active.as_mut().filter(|a| a.hello_done)
            {
                a.gap += skip_ahead(
                    &mut a.output_sent,
                    a.replay_end,
                    &output,
                    config.live_backlog,
                );
            }
            let connected = active.as_ref().is_some_and(|a| a.hello_done);
            if connected && output.free() == 0 {
                full_since.get_or_insert_with(Instant::now);
            } else {
                full_since = None;
            }
            // With `Drop` the job never waits, so it always counts as away: the oldest output is
            // forgotten instead.
            let away = config.slow_client == SlowClient::Drop
                || !connected
                || full_since.is_some_and(|t| t.elapsed() >= config.away_after);
            let read_len = if away {
                buf.len()
            } else {
                output.free().min(buf.len())
            };
            let away_deadline = full_since.filter(|_| !away).map(|t| t + config.away_after);
            let exit_due = match (shell.as_ref(), active.as_ref()) {
                (Some(s), Some(a)) => ready_for_exit(s, a, &output),
                _ => false,
            };
            let send_due = active.as_ref().is_some_and(|a| {
                a.hello_done
                    && (a.pong_due
                        || a.gap > 0
                        || a.input_acked < input_received
                        || a.output_sent < output.end()
                        || exit_due)
            });
            let want_input = pending_offset >= pending_input.len();
            // a deliberate detach waits for max_detached, a lost connection for detached_timeout
            let limit = if detached_on_purpose {
                config.max_detached
            } else {
                config.detached_timeout
            };
            let limit = if ever_connected {
                limit
            } else {
                Some(limit.map_or(config.first_client_timeout, |l| {
                    l.min(config.first_client_timeout)
                }))
            };
            let deadline = match (active.is_none(), limit, detached_since) {
                (true, Some(limit), Some(since)) => Some(since + limit),
                _ => None,
            };
            let grace_deadline = exit_lost
                .filter(|_| active.is_none())
                .map(|since| since + config.exit_grace);
            if control_rx.is_empty()
                && let Some(size) = pending_resize.take()
                && let Some(s) = shell.as_ref()
            {
                let _ = s.writer.resize(size);
            }
            let event = {
                let (reader, writer, exit) = match shell.as_mut() {
                    Some(s) => (
                        (!s.reader_eof && read_len > 0).then_some(&mut s.reader),
                        (!want_input).then_some(&mut s.writer),
                        s.exit.as_mut(),
                    ),
                    None => (None, None, None),
                };
                let (input, out) = match active.as_mut() {
                    Some(a) if a.hello_done => (
                        want_input.then_some(&mut a.input),
                        send_due.then(|| a.out.clone()),
                    ),
                    _ => (None, None),
                };

                tokio::select! {
                    conn = conns.recv() => Event::Connection(conn),
                    command = async { match commands.as_mut() { Some(rx) => rx.recv().await, None => pending().await } } => Event::Command(command),
                    tagged = control_rx.recv() => Event::Control(tagged),
                    data = async { match input { Some(rx) => rx.recv().await, None => pending().await } } => Event::Input(data),
                    n = async { match reader { Some(r) => r.read(&mut buf[..read_len]).await, None => pending().await } } => Event::ShellOutput(n),
                    n = async { match writer { Some(w) => w.write(&pending_input[pending_offset..]).await, None => pending().await } } => Event::InputWritten(n),
                    status = async { match exit { Some(e) => e.await, None => pending().await } } => Event::Exited(status),
                    permit = async { match out { Some(tx) => tx.reserve_owned().await, None => pending().await } } => Event::CanSend(permit),
                    _ = async { match deadline { Some(d) => sleep_until(d).await, None => pending().await } } => Event::DetachedTimeout,
                    _ = async { match grace_deadline { Some(d) => sleep_until(d).await, None => pending().await } } => Event::ExitGrace,
                    _ = async { match away_deadline { Some(d) => sleep_until(d).await, None => pending().await } } => Event::AwayTimer,
                    stream = async { match agent_listener.as_ref() { Some((l, _)) => l.accept().await.map(|(s, _)| s), None => pending().await } } => Event::AgentConnected(stream),
                }
            };

            match event {
                Event::Command(None) => commands = None,
                Event::Command(Some(SessionCommand::Kill)) => {
                    if let Some(Running {
                        reader,
                        writer,
                        exit,
                        ..
                    }) = shell.take()
                    {
                        // closing the terminal makes the kernel send SIGHUP to the shell
                        drop(reader);
                        drop(writer);
                        if let Some(exit) = exit {
                            let _ = tokio::time::timeout(Duration::from_secs(5), exit).await;
                        }
                    }
                    if let Some(a) = active.as_ref().filter(|a| a.hello_done) {
                        let _ = a.out.try_send(ServerFrame::Exit(WireExit::Signal(1)));
                    }
                    return SessionEnd::Killed;
                }
                Event::Connection(None) => return SessionEnd::Closed,
                Event::Connection(Some(conn)) => {
                    generation += 1;
                    active = Some(start_connection(
                        conn,
                        generation,
                        control_tx.clone(),
                        config.hello_timeout,
                        config.lost_after,
                    ));
                    detached_since = None;
                    detached_on_purpose = false;
                }
                Event::DetachedTimeout => return SessionEnd::DetachedTimeout,
                // the next turn of the loop sees that the client is away now
                Event::AwayTimer => {}
                Event::Control(None) => unreachable!("the actor holds a sender"),
                Event::Control(Some(Tagged {
                    generation: g,
                    control,
                })) => {
                    if active.as_ref().map(|a| a.generation) != Some(g) {
                        continue;
                    }
                    match control {
                        Control::Hello(hello) => {
                            let view = ServerView {
                                shell_started: shell.is_some(),
                                output: &output,
                                input_received,
                            };
                            let a = active.as_mut().expect("checked above");
                            // The shell was started before any client came, so the first
                            // `Start` joins it: nothing was consumed, and the size is applied.
                            let joins = config.prestart.is_some()
                                && !ever_connected
                                && input_received == 0
                                && output.start() == 0;
                            let negotiated = match hello {
                                Hello::Start { size, .. } if joins => Ok((
                                    ssj_proto::Welcome {
                                        input_offset: 0,
                                        output_offset: 0,
                                    },
                                    Action::Resize(size),
                                )),
                                hello => negotiate(hello, &view),
                            };
                            match negotiated {
                                Err(code) => {
                                    let _ = a.out.try_send(ServerFrame::Error(code));
                                    active = None;
                                    detached_since = Some(Instant::now());
                                }
                                Ok((welcome, action)) => {
                                    match action {
                                        Action::Spawn { size, env, agent } => {
                                            match start_shell(
                                                &backend,
                                                &config,
                                                size,
                                                &env,
                                                agent,
                                                &mut agent_listener,
                                            )
                                            .await
                                            {
                                                Ok(running) => shell = Some(running),
                                                Err(err) => {
                                                    let _ = a.out.try_send(ServerFrame::Error(
                                                        ErrorCode::Internal,
                                                    ));
                                                    return SessionEnd::Failed(err);
                                                }
                                            }
                                        }
                                        Action::Resize(size) => {
                                            if let Some(s) = shell.as_ref() {
                                                let _ = s.writer.resize(size);
                                            }
                                        }
                                    }
                                    // the size of the hello is newer than any waiting one
                                    pending_resize = None;
                                    a.hello_done = true;
                                    ever_connected = true;
                                    if let Some(events) = &events {
                                        let _ = events.send(SessionEvent::Connected);
                                    }
                                    a.output_sent = welcome.output_offset;
                                    a.replay_end = output.end();
                                    a.input_acked = welcome.input_offset;
                                    let _ = a.out.try_send(ServerFrame::Welcome(welcome));
                                }
                            }
                        }
                        Control::Resize(size) => pending_resize = Some(size),
                        Control::OutputAck(offset) => {
                            if output.ack(offset).is_err() {
                                violation(&mut active, &events, ErrorCode::Violation);
                                detached_since = Some(Instant::now());
                            }
                        }
                        control @ (Control::Close | Control::Lost) => {
                            let finished = shell.as_ref().is_some_and(|s| s.exit_queued);
                            lose(&mut active, &events);
                            detached_since = Some(Instant::now());
                            if finished && matches!(control, Control::Close) {
                                let status = shell
                                    .and_then(|s| s.status)
                                    .expect("exit is queued after the status is known");
                                return SessionEnd::ShellExited(status);
                            }
                            if let Some(s) = shell.as_mut().filter(|_| finished) {
                                // the status may not have arrived: it goes out again to the next
                                // connection, and a close confirms it
                                s.exit_queued = false;
                                exit_lost = Some(Instant::now());
                            }
                        }
                        Control::Ping => {
                            if let Some(a) = active.as_mut() {
                                a.pong_due = a.hello_done;
                            }
                        }
                        Control::AgentOffer => {
                            if let Some(a) = active.as_mut().filter(|a| a.hello_done)
                                && agent_listener.is_some()
                            {
                                a.agent = Some(Channels::new());
                            }
                        }
                        Control::Agent(id, data) => {
                            if let Some(a) = active.as_mut()
                                && let Some(channels) = a.agent.as_mut()
                                && channels.is_open(id)
                                && !channels.send(id, data)
                            {
                                tell_channel_closed(&a.out, id);
                            }
                        }
                        Control::AgentClose(id) => {
                            if let Some(channels) = active.as_mut().and_then(|a| a.agent.as_mut()) {
                                channels.close(id);
                            }
                        }
                        Control::Detach => {
                            active = None;
                            detached_since = Some(Instant::now());
                            detached_on_purpose = true;
                            if let Some(events) = &events {
                                let _ = events.send(SessionEvent::Detached);
                            }
                        }
                        Control::Violation(code) => {
                            violation(&mut active, &events, code);
                            detached_since = Some(Instant::now());
                        }
                    }
                }
                Event::AgentConnected(Err(_)) => {
                    // for example out of descriptors: do not spin on it
                    sleep(Duration::from_millis(100)).await;
                }
                Event::AgentConnected(Ok(stream)) => {
                    // without a client that serves the agent, the program sees a closed socket
                    if let Some(a) = active.as_mut()
                        && let Some(channels) = a.agent.as_mut()
                    {
                        next_channel = next_channel.wrapping_add(1);
                        let out = a.out.clone();
                        channels.open(next_channel, stream, move |id, piece| {
                            let out = out.clone();
                            async move {
                                let frame = match piece {
                                    Piece::Open => ServerFrame::AgentOpen(id),
                                    Piece::Data(data) => ServerFrame::Agent(id, data),
                                    Piece::Closed => ServerFrame::AgentClose(id),
                                };
                                out.send(frame).await.is_ok()
                            }
                        });
                    }
                }
                Event::Input(None) => {}
                Event::Input(Some(data)) => {
                    pending_input = data;
                    pending_offset = 0;
                    pending_budget = active.as_ref().map(|a| a.budget.clone());
                }
                Event::InputWritten(Err(_)) => {
                    // the shell closed its input, so the output side will report the end
                    release_input(&mut pending_input, &mut pending_offset, &mut pending_budget);
                }
                Event::InputWritten(Ok(n)) => {
                    pending_offset += n;
                    input_received += n as u64;
                    if pending_offset >= pending_input.len() {
                        release_input(&mut pending_input, &mut pending_offset, &mut pending_budget);
                    }
                }
                Event::ShellOutput(result) => {
                    let s = shell.as_mut().expect("read only while a shell exists");
                    match result {
                        Ok(0) | Err(_) => s.reader_eof = true,
                        Ok(first) => {
                            // A terminal hands out its data in small pieces (1 KiB on macOS), and
                            // a pass through this loop for each would cap the throughput. So take
                            // what is ready now, without waiting, up to the room that is left.
                            let mut n = first;
                            while n < read_len {
                                match read_ready(&mut s.reader, &mut buf[n..read_len]) {
                                    Some(Ok(more)) if more > 0 => n += more,
                                    Some(_) => {
                                        s.reader_eof = true;
                                        break;
                                    }
                                    None => break,
                                }
                            }
                            let forgotten = output.push_evicting(&buf[..n]);
                            debug_assert!(
                                forgotten == 0 || away,
                                "output dropped under backpressure"
                            );
                            // a connection that was still owed the forgotten bytes cannot go on:
                            // its client resumes on a new connection and learns what it missed
                            let owed = active
                                .as_ref()
                                .is_some_and(|a| a.hello_done && a.output_sent < output.start());
                            // with `Drop` the connection goes on and is told about the gap
                            if forgotten > 0 && owed && config.slow_client == SlowClient::Wait {
                                lose(&mut active, &events);
                                detached_since = Some(Instant::now());
                            }
                        }
                    }
                }
                Event::Exited(result) => {
                    let s = shell.as_mut().expect("exit only while a shell exists");
                    s.exit = None;
                    match result {
                        Ok(status) => s.status = Some(status),
                        Err(err) => return SessionEnd::Failed(err),
                    }
                }
                Event::ExitGrace => {
                    // nobody came back for the status: the client has it or never will
                    let status = shell
                        .and_then(|s| s.status)
                        .expect("the grace period starts after the status is known");
                    return SessionEnd::ShellExited(status);
                }
                Event::CanSend(Err(_)) => {
                    lose(&mut active, &events);
                    detached_since = Some(Instant::now());
                }
                Event::CanSend(Ok(permit)) => {
                    let Some(a) = active.as_mut() else { continue };
                    let frame = if a.pong_due {
                        a.pong_due = false;
                        ServerFrame::Pong
                    } else if a.input_acked < input_received {
                        a.input_acked = input_received;
                        ServerFrame::InputAck(input_received)
                    } else if a.gap > 0 {
                        ServerFrame::Gap(std::mem::take(&mut a.gap))
                    } else if a.output_sent < output.end() {
                        let chunk = output
                            .read_from(a.output_sent, MAX_DATA_LEN)
                            .expect("sent offset lies inside the buffer");
                        // How much the client has not taken yet: what waits to be sent and what
                        // was sent and is not acknowledged. Compressing costs time, and it only
                        // pays when the link is what holds the output back. A link that keeps
                        // up has little of this, because the acknowledgements come back at once.
                        let waiting = output.end() - output.start();
                        a.output_sent += chunk.len() as u64;
                        let packed = (config.compress
                            && waiting >= COMPRESS_BACKLOG
                            && chunk.len() >= COMPRESS_MIN_CHUNK)
                            .then(|| compressor.compress(&chunk))
                            .flatten();
                        match packed {
                            Some(packed) => ServerFrame::Compressed(packed),
                            None => ServerFrame::Output(chunk),
                        }
                    } else if let Some(s) = shell.as_mut().filter(|s| ready_for_exit(s, a, &output))
                    {
                        s.exit_queued = true;
                        ServerFrame::Exit(wire_exit(s.status.expect("ready implies a status")))
                    } else {
                        continue;
                    };
                    permit.send(frame);
                }
            }
        }
    }
}

/// Moves the send position of a connection forward when too much output waits for it, or when
/// the buffer has forgotten what it was to send. Returns the number of bytes skipped.
///
/// Output from before `replay_end` is not counted as waiting: a client that joins gets the replay
/// at the speed of its link, and only what the shell writes afterwards is limited to `live` bytes.
fn skip_ahead(sent: &mut u64, replay_end: u64, output: &ReplayBuffer, live: usize) -> u64 {
    let end = output.end();
    let mut target = (*sent).max(output.start());
    let waiting = end - target.max(replay_end).min(end);
    if waiting > live as u64 {
        target = end - live as u64;
    }
    if target > *sent {
        // do not start inside a character of several bytes
        let head = output.read_from(target, 3).unwrap_or_default();
        target += head.iter().take_while(|b| **b & 0xc0 == 0x80).count() as u64;
        let skipped = target - *sent;
        *sent = target;
        return skipped;
    }
    0
}

/// Reads what is ready now, or returns `None` when nothing is, without waiting.
fn read_ready<R: AsyncRead + Unpin>(reader: &mut R, buf: &mut [u8]) -> Option<io::Result<usize>> {
    use std::task::{Context, Poll, Waker};
    let mut read_buf = tokio::io::ReadBuf::new(buf);
    // the waker is not needed: the select loop polls the reader again with its own
    let mut cx = Context::from_waker(Waker::noop());
    match std::pin::Pin::new(reader).poll_read(&mut cx, &mut read_buf) {
        Poll::Ready(Ok(())) => Some(Ok(read_buf.filled().len())),
        Poll::Ready(Err(err)) => Some(Err(err)),
        Poll::Pending => None,
    }
}

/// Drops the chunk that was written and returns its bytes to the budget of the connection.
fn release_input(chunk: &mut Vec<u8>, offset: &mut usize, budget: &mut Option<Arc<Semaphore>>) {
    if let Some(budget) = budget.take() {
        budget.add_permits(chunk.len());
    }
    chunk.clear();
    *offset = 0;
}

/// The shell is gone, all its output is read and queued, and the exit was not announced yet.
fn ready_for_exit<S: Shell>(shell: &Running<S>, active: &Active, output: &ReplayBuffer) -> bool {
    shell.status.is_some()
        && shell.reader_eof
        && !shell.exit_queued
        && active.output_sent == output.end()
}

fn wire_exit(status: ExitStatus) -> WireExit {
    use std::os::unix::process::ExitStatusExt;
    match (status.code(), status.signal()) {
        (Some(code), _) => WireExit::Code(code as u8),
        (None, Some(signal)) => WireExit::Signal(signal as u8),
        (None, None) => WireExit::Code(255),
    }
}

/// Ends the connection and reports it if the client had completed its handshake.
fn lose(active: &mut Option<Active>, events: &Option<mpsc::UnboundedSender<SessionEvent>>) {
    let was_connected = active.take().is_some_and(|a| a.hello_done);
    if let (true, Some(events)) = (was_connected, events) {
        let _ = events.send(SessionEvent::Disconnected);
    }
}

fn violation(
    active: &mut Option<Active>,
    events: &Option<mpsc::UnboundedSender<SessionEvent>>,
    code: ErrorCode,
) {
    if let Some(a) = active.as_ref() {
        let _ = a.out.try_send(ServerFrame::Error(code));
    }
    lose(active, events);
}

/// Makes the socket for the agent of the client. Without it the shell has no agent.
fn listen_for_agent(path: &Path) -> Option<(UnixListener, SocketFile)> {
    use std::os::unix::fs::PermissionsExt;
    // a leftover of a server that did not end cleanly
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path).ok()?;
    let file = SocketFile(path.to_owned());
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).ok()?;
    Some((listener, file))
}

/// Tells the client that channel `id` is closed, without waiting for room in the queue.
fn tell_channel_closed(out: &mpsc::Sender<ServerFrame>, id: u32) {
    let out = out.clone();
    tokio::spawn(async move {
        let _ = out.send(ServerFrame::AgentClose(id)).await;
    });
}

/// Starts the shell, and the socket for the agent of the client when it is asked for.
async fn start_shell<B: Backend>(
    backend: &B,
    config: &SessionConfig,
    size: WindowSize,
    client_env: &Env,
    agent: bool,
    agent_listener: &mut Option<(UnixListener, SocketFile)>,
) -> io::Result<Running<B::Shell>> {
    if agent && let Some(path) = &config.agent_socket {
        *agent_listener = listen_for_agent(path);
    }
    let socket = agent_listener.as_ref().and(config.agent_socket.as_deref());
    spawn_shell(backend, config, size, client_env, socket).await
}

async fn spawn_shell<B: Backend>(
    backend: &B,
    config: &SessionConfig,
    size: WindowSize,
    client_env: &Env,
    agent_socket: Option<&Path>,
) -> io::Result<Running<B::Shell>> {
    let (accepted, _dropped) = config.accept_env.apply(client_env);
    let mut env = config.template.env.clone();
    for (name, value) in accepted.iter() {
        env.retain(|(existing, _)| existing != name);
        env.push((name.into(), value.into()));
    }
    if let Some(socket) = agent_socket {
        env.retain(|(existing, _)| existing != "SSH_AUTH_SOCK");
        env.push(("SSH_AUTH_SOCK".into(), socket.into()));
    }
    let spec = SpawnSpec {
        program: config.template.program.clone(),
        args: config.template.args.clone(),
        env,
        cwd: config.template.cwd.clone(),
        size,
    };
    let parts = backend.spawn(&config.id, spec).await?.split();
    Ok(Running {
        reader: parts.reader,
        writer: parts.writer,
        exit: Some(parts.exit),
        status: None,
        reader_eof: false,
        exit_queued: false,
    })
}

fn start_connection(
    conn: Connection,
    generation: u64,
    control: mpsc::Sender<Tagged>,
    hello_timeout: Duration,
    lost_after: Duration,
) -> Active {
    let (out_tx, out_rx) = mpsc::channel(OUT_QUEUE);
    let (input_tx, input_rx) = mpsc::unbounded_channel();
    let budget = Arc::new(Semaphore::new(INPUT_BUDGET));
    let reader = tokio::spawn(read_connection(
        generation,
        conn.reader,
        control.clone(),
        input_tx,
        budget.clone(),
        Instant::now() + hello_timeout,
        lost_after,
    ));
    let writer = tokio::spawn(write_connection(generation, conn.writer, out_rx, control));
    Active {
        generation,
        out: out_tx,
        input: input_rx,
        budget,
        hello_done: false,
        pong_due: false,
        output_sent: 0,
        input_acked: 0,
        replay_end: 0,
        gap: 0,
        agent: None,
        _reader: AbortOnDrop(reader),
        writer: Some(writer),
    }
}

async fn read_connection(
    generation: u64,
    mut reader: Box<dyn AsyncRead + Send + Unpin>,
    control: mpsc::Sender<Tagged>,
    input: mpsc::UnboundedSender<Vec<u8>>,
    budget: Arc<Semaphore>,
    hello_deadline: Instant,
    lost_after: Duration,
) {
    let send = |control_event| async {
        let _ = control
            .send(Tagged {
                generation,
                control: control_event,
            })
            .await;
    };
    let mut pending = Vec::new();
    let mut chunk = vec![0u8; READ_CHUNK];
    let mut got_hello = false;
    loop {
        let read = if got_hello {
            match timeout(lost_after, reader.read(&mut chunk)).await {
                Ok(result) => result,
                Err(_) => return send(Control::Lost).await,
            }
        } else {
            match timeout_at(hello_deadline, reader.read(&mut chunk)).await {
                Ok(result) => result,
                Err(_) => return send(Control::Violation(ErrorCode::BadHello)).await,
            }
        };
        match read {
            Ok(0) | Err(_) => return send(Control::Lost).await,
            Ok(n) => pending.extend_from_slice(&chunk[..n]),
        }
        loop {
            let (frame, used) = match decode_client(&pending) {
                Ok(Some(decoded)) => decoded,
                Ok(None) => break,
                Err(_) => {
                    let code = if got_hello {
                        ErrorCode::Violation
                    } else {
                        ErrorCode::BadHello
                    };
                    return send(Control::Violation(code)).await;
                }
            };
            pending.drain(..used);
            // compressed input is the same as plain input once it is inflated
            let frame = match frame {
                ClientFrame::Compressed(packed) if got_hello => match inflate(&packed) {
                    Ok(data) => ClientFrame::Input(data),
                    Err(_) => return send(Control::Violation(ErrorCode::Violation)).await,
                },
                other => other,
            };
            match (frame, got_hello) {
                (ClientFrame::Hello(hello), false) => {
                    got_hello = true;
                    send(Control::Hello(hello)).await;
                }
                (ClientFrame::Hello(_), true) => {
                    return send(Control::Violation(ErrorCode::Violation)).await;
                }
                (_, false) => return send(Control::Violation(ErrorCode::BadHello)).await,
                (ClientFrame::Input(data), true) => {
                    // waits only for a client that sends far more than it may
                    match budget.acquire_many(data.len() as u32).await {
                        Ok(permit) => permit.forget(),
                        Err(_) => return,
                    }
                    if input.send(data).is_err() {
                        return;
                    }
                }
                (ClientFrame::Resize(size), true) => send(Control::Resize(size)).await,
                (ClientFrame::OutputAck(offset), true) => send(Control::OutputAck(offset)).await,
                (ClientFrame::Close, true) => return send(Control::Close).await,
                (ClientFrame::Detach, true) => return send(Control::Detach).await,
                (ClientFrame::Ping, true) => send(Control::Ping).await,
                (ClientFrame::AgentOffer, true) => send(Control::AgentOffer).await,
                (ClientFrame::Agent(id, data), true) => send(Control::Agent(id, data)).await,
                (ClientFrame::AgentClose(id), true) => send(Control::AgentClose(id)).await,
                // inflated above, and refused before the hello
                (ClientFrame::Compressed(_), _) => {
                    return send(Control::Violation(ErrorCode::BadHello)).await;
                }
            }
        }
        // a header that announces more than a frame may hold is rejected by the decoder, so
        // the pending bytes stay below one frame plus one read
        debug_assert!(pending.len() < MAX_FRAME_LEN + 4 + READ_CHUNK);
    }
}

async fn write_connection(
    generation: u64,
    mut writer: Box<dyn AsyncWrite + Send + Unpin>,
    mut frames: mpsc::Receiver<ServerFrame>,
    control: mpsc::Sender<Tagged>,
) {
    let mut bytes = Vec::new();
    while let Some(frame) = frames.recv().await {
        bytes.clear();
        if frame.encode(&mut bytes).is_err() {
            break;
        }
        // no timeout: the stream may legitimately stall while the network is down
        if writer.write_all(&bytes).await.is_err() || writer.flush().await.is_err() {
            let _ = control
                .send(Tagged {
                    generation,
                    control: Control::Lost,
                })
                .await;
            return;
        }
    }
    let _ = writer.shutdown().await;
}
