//! The session of one server process: the shell, its buffered output, and the current connection.
//!
//! A single task owns all state. Connections come in as byte streams, so the logic does not
//! depend on QUIC. Reading the shell, writing to it, and talking to the client never wait for
//! each other, which is what prevents deadlocks when a large paste meets a chatty program.

use std::ffi::OsString;
use std::future::pending;
use std::io;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::time::Duration;

use ssj_proto::frame::{MAX_DATA_LEN, MAX_FRAME_LEN};
use ssj_proto::resume::{Action, ServerView, negotiate};
use ssj_proto::{
    ClientFrame, Env, EnvFilter, ErrorCode, ExitStatus as WireExit, Hello, ReplayBuffer,
    ServerFrame, SessionId, WindowSize, decode_client,
};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep_until, timeout_at};

use crate::backend::{Backend, Shell, ShellWriter, SpawnSpec};

/// Bytes of the shell output read at once.
const READ_CHUNK: usize = 16 * 1024;
/// Frames waiting for the writer of the connection.
const OUT_QUEUE: usize = 2;
/// Input bytes a connection may have queued for the shell.
///
/// The reader must keep parsing while the shell is busy, because acknowledgements of output are
/// in the same byte stream as the input and are what lets the shell continue. The client limits
/// its unacknowledged input to less than this, so the reader does not block on the budget.
pub const INPUT_BUDGET: usize = 2 * 1024 * 1024;

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

#[derive(Clone, Debug)]
pub struct SessionConfig {
    pub id: SessionId,
    pub template: SpawnTemplate,
    /// Variables of the client that reach the shell.
    pub accept_env: EnvFilter,
    /// Output bytes kept until the client acknowledges them.
    pub replay_capacity: usize,
    /// End of the session after this long without a connection. `None` waits forever.
    pub detached_timeout: Option<Duration>,
    /// Time a new connection has to send its first frame.
    pub hello_timeout: Duration,
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
}

/// Changes of the state of the session that the transport needs to know about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionEvent {
    /// The client left on purpose. The transport closes its endpoint and drops the pinned client.
    Detached,
}

/// Manages the shell of one session across the connections of its client.
pub struct Session<B: Backend> {
    backend: B,
    config: SessionConfig,
    events: Option<mpsc::UnboundedSender<SessionEvent>>,
}

enum Control {
    Hello(Hello),
    Resize(WindowSize),
    OutputAck(u64),
    Close,
    Detach,
    Ping,
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
        }
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
        // a deliberate detach disables the timeout until the next connection
        let mut detached_on_purpose = false;
        let mut buf = vec![0u8; READ_CHUNK];

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
            }

            let free = output.free().min(buf.len());
            let exit_due = match (shell.as_ref(), active.as_ref()) {
                (Some(s), Some(a)) => ready_for_exit(s, a, &output),
                _ => false,
            };
            let send_due = active.as_ref().is_some_and(|a| {
                a.hello_done
                    && (a.pong_due
                        || a.input_acked < input_received
                        || a.output_sent < output.end()
                        || exit_due)
            });
            let want_input = pending_offset >= pending_input.len();
            let deadline = match (
                active.is_none(),
                detached_on_purpose,
                config.detached_timeout,
                detached_since,
            ) {
                (true, false, Some(limit), Some(since)) => Some(since + limit),
                _ => None,
            };
            let event = {
                let (reader, writer, exit) = match shell.as_mut() {
                    Some(s) => (
                        (!s.reader_eof && free > 0).then_some(&mut s.reader),
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
                    tagged = control_rx.recv() => Event::Control(tagged),
                    data = async { match input { Some(rx) => rx.recv().await, None => pending().await } } => Event::Input(data),
                    n = async { match reader { Some(r) => r.read(&mut buf[..free]).await, None => pending().await } } => Event::ShellOutput(n),
                    n = async { match writer { Some(w) => w.write(&pending_input[pending_offset..]).await, None => pending().await } } => Event::InputWritten(n),
                    status = async { match exit { Some(e) => e.await, None => pending().await } } => Event::Exited(status),
                    permit = async { match out { Some(tx) => tx.reserve_owned().await, None => pending().await } } => Event::CanSend(permit),
                    _ = async { match deadline { Some(d) => sleep_until(d).await, None => pending().await } } => Event::DetachedTimeout,
                }
            };

            match event {
                Event::Connection(None) => return SessionEnd::Closed,
                Event::Connection(Some(conn)) => {
                    generation += 1;
                    active = Some(start_connection(
                        conn,
                        generation,
                        control_tx.clone(),
                        config.hello_timeout,
                    ));
                    detached_since = None;
                    detached_on_purpose = false;
                }
                Event::DetachedTimeout => return SessionEnd::DetachedTimeout,
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
                            match negotiate(hello, &view) {
                                Err(code) => {
                                    let _ = a.out.try_send(ServerFrame::Error(code));
                                    active = None;
                                    detached_since = Some(Instant::now());
                                }
                                Ok((welcome, action)) => {
                                    match action {
                                        Action::Spawn { size, env } => {
                                            match spawn_shell(&backend, &config, size, &env).await {
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
                                    a.hello_done = true;
                                    a.output_sent = welcome.output_offset;
                                    a.input_acked = welcome.input_offset;
                                    let _ = a.out.try_send(ServerFrame::Welcome(welcome));
                                }
                            }
                        }
                        Control::Resize(size) => {
                            if let Some(s) = shell.as_ref() {
                                let _ = s.writer.resize(size);
                            }
                        }
                        Control::OutputAck(offset) => {
                            if output.ack(offset).is_err() {
                                violation(&mut active, ErrorCode::Violation);
                                detached_since = Some(Instant::now());
                            }
                        }
                        Control::Close | Control::Lost => {
                            let finished = shell.as_ref().is_some_and(|s| s.exit_queued);
                            active = None;
                            detached_since = Some(Instant::now());
                            if finished {
                                let status = shell
                                    .and_then(|s| s.status)
                                    .expect("exit is queued after the status is known");
                                return SessionEnd::ShellExited(status);
                            }
                        }
                        Control::Ping => {
                            if let Some(a) = active.as_mut() {
                                a.pong_due = a.hello_done;
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
                            violation(&mut active, code);
                            detached_since = Some(Instant::now());
                        }
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
                        Ok(n) => {
                            let stored = output.push(&buf[..n]);
                            debug_assert_eq!(stored, n);
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
                Event::CanSend(Err(_)) => {
                    active = None;
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
                    } else if a.output_sent < output.end() {
                        let chunk = output
                            .read_from(a.output_sent, MAX_DATA_LEN)
                            .expect("sent offset lies inside the buffer");
                        a.output_sent += chunk.len() as u64;
                        ServerFrame::Output(chunk)
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

fn violation(active: &mut Option<Active>, code: ErrorCode) {
    if let Some(a) = active.as_ref() {
        let _ = a.out.try_send(ServerFrame::Error(code));
    }
    *active = None;
}

async fn spawn_shell<B: Backend>(
    backend: &B,
    config: &SessionConfig,
    size: WindowSize,
    client_env: &Env,
) -> io::Result<Running<B::Shell>> {
    let (accepted, _dropped) = config.accept_env.apply(client_env);
    let mut env = config.template.env.clone();
    for (name, value) in accepted.iter() {
        env.retain(|(existing, _)| existing != name);
        env.push((name.into(), value.into()));
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
            reader.read(&mut chunk).await
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
