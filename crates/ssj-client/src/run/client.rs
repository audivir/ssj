//! The client of a running session: the loop that passes bytes, acknowledges, and reconnects.

mod link;

use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

use quinn::Endpoint;
use ssj_proto::compress::{Compressor, inflate};
use ssj_proto::{ClientFrame, ExitStatus, Hello, ReplayBuffer, ServerFrame, WindowSize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{interval, sleep, timeout};

use super::wire::{FrameReader, Out, hello_name, server_frame_name, write_frames};
use super::{Announce, FLUSH_TIMEOUT, Finished, INPUT_BUFFER, Io, Params, Reader, Tier};
use crate::agent::Forward;
use crate::debounce::Debounce;
use crate::echo::Echo;
use crate::netwatch::Cause;
use crate::status::dropped_notice;
use crate::tracker::Tracker;
use crate::vlog;

/// Pauses between attempts to reconnect, the last one repeats.
const BACKOFF: [Duration; 5] = [
    Duration::from_millis(250),
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(3),
];
/// How long after the last debug line on the terminal the screen is drawn again.
const REDRAW_AFTER_LOG: Duration = Duration::from_millis(400);
/// Time QUIC gets to open on a new network while the session runs over a stream transport.
const UPGRADE_TIMEOUT: Duration = Duration::from_millis(1500);
/// A new lookup of the server address happens before every attempt after this many failed ones.
const RESOLVE_EVERY: usize = 3;
/// A resize is sent when no newer one has come for this long.
const RESIZE_QUIET: Duration = Duration::from_millis(50);
const PING_EVERY: Duration = Duration::from_secs(1);
/// Output that is acknowledged at once. Less waits for a frame that goes out anyway.
const ACK_BYTES: u64 = 4096;
/// Input is compressed only while at least this many bytes wait for the server to take them.
const COMPRESS_BACKLOG: u64 = 256 * 1024;
/// Chunks shorter than this are not worth compressing.
const COMPRESS_MIN_CHUNK: usize = 1024;
/// Most output that is written to the terminal in one go.
const OUTPUT_BATCH: usize = 256 * 1024;
/// How long a status waits for a sequence of the remote side to end before it cancels it.
const SEQUENCE_PATIENCE: Duration = Duration::from_secs(2);
/// The client says that the server is silent after four round trips, but not sooner than this
/// and not later than the maximum, so that a good link reports a stall quickly and a slow link
/// does not report one all the time.
const STALL_MIN: Duration = Duration::from_secs(2);
const STALL_MAX: Duration = Duration::from_secs(10);
/// A stream that has heard nothing for this long gets a spare: a second connection is opened
/// beside it, and the session moves to whichever works. Giving the stream up only after
/// [`super::STREAM_DEAD_AFTER`] would leave a flaky link (a train) without a session for that long, and
/// the stream itself may be in the long backoff of TCP when the network is back.
const SPARE_AFTER: Duration = Duration::from_secs(3);
/// Time a spare gets to open. A connection that was started on a dead path has lost its first
/// packets and retries them after 1 and 3 seconds, and then needs the login: it opens about 6
/// seconds after it was started, so a shorter time would end the spares that are about to work.
/// The fresh ones that come every second find a returning network sooner.
const SPARE_TIMEOUT: Duration = Duration::from_secs(10);
/// Spares that open at the same time. A connection started on a dead path loses its first packets
/// and may not open for seconds, so a new one starts every second while the stream lags, and the
/// first to open wins (the same idea as asking two servers and taking the faster answer).
const SPARES_AT_ONCE: usize = 6;
/// Pause after a spare that failed outright, so that a broken login does not become a loop.
const SPARE_PAUSE: Duration = Duration::from_secs(2);
/// A change of network produces many notifications. One reaction follows when they stop.
const CHANGE_QUIET: Duration = Duration::from_millis(300);
/// Time the server gets to answer after the client moved to a new network path, before the client
/// starts a new connection. A wake from sleep leaves less hope for the old path.
const PROBE_AFTER_CHANGE: Duration = Duration::from_secs(1);
const PROBE_AFTER_WAKE: Duration = Duration::from_secs(1);

/// The next cause from the watch, or never if there is none.
async fn next_cause(net: &mut Option<mpsc::UnboundedReceiver<Cause>>) -> Option<Cause> {
    match net {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

fn exit_code(status: ExitStatus) -> u8 {
    match status {
        ExitStatus::Code(code) => code,
        ExitStatus::Signal(signal) => 128u8.saturating_add(signal),
    }
}

/// One connection to the server with its reader and writer.
struct Link {
    /// The QUIC connection, if the link is one.
    quic: Option<quinn::Connection>,
    out: mpsc::UnboundedSender<ClientFrame>,
    writer: JoinHandle<()>,
    reader: FrameReader,
    last_heard: Instant,
    /// When the oldest ping that no pong has answered went out. Frames of the server that arrive
    /// meanwhile show that its direction works and say nothing about ours.
    ping_since: Option<Instant>,
    _guard: Option<Box<dyn Send + Sync>>,
}

impl Link {
    fn new(
        quic: Option<quinn::Connection>,
        out: Out,
        reader: Reader,
        guard: Option<Box<dyn Send + Sync>>,
    ) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            quic,
            out: tx,
            writer: tokio::spawn(write_frames(out, rx)),
            reader: FrameReader::new(reader),
            last_heard: Instant::now(),
            ping_since: None,
            _guard: guard,
        }
    }

    fn send(&self, frame: ClientFrame) {
        let _ = self.out.send(frame);
    }

    /// Sends a ping and remembers when, until the pong comes.
    fn ping(&mut self) {
        self.ping_since.get_or_insert_with(Instant::now);
        self.send(ClientFrame::Ping);
    }

    /// Lets the writer deliver what is queued, then ends the link.
    async fn finish(self) {
        drop(self.out);
        let _ = timeout(FLUSH_TIMEOUT * 2, self.writer).await;
    }
}

/// Makes the program in the session draw its screen again, by changing the size of the terminal
/// for a moment: debug lines that were written over the screen are then gone.
async fn redraw(size: WindowSize, out: &mpsc::UnboundedSender<ClientFrame>) {
    let narrower =
        WindowSize::new(size.rows(), size.cols().saturating_sub(1).max(1)).filter(|n| *n != size);
    if let Some(narrower) = narrower {
        let _ = out.send(ClientFrame::Resize(narrower));
        sleep(Duration::from_millis(60)).await;
    }
    let _ = out.send(ClientFrame::Resize(size));
}

enum Attempt {
    Linked(Link),
    /// Try again at once on the same transport.
    Again,
    /// Try again later.
    Retry,
    /// The session cannot continue.
    Fatal(String),
}

enum Pumped {
    Done(Finished),
    Lost,
    /// A better connection opened while this one worked: go on over it, as after a reconnect.
    Switch(Link, usize),
}

/// QUIC being tried on a new network, within a time limit.
type Upgrade =
    Pin<Box<dyn Future<Output = Result<Result<Link, String>, tokio::time::error::Elapsed>> + Send>>;

/// A connection that is being opened.
type LinkFuture = Pin<Box<dyn Future<Output = Result<Link, String>> + Send>>;

struct Client {
    /// The UDP socket, made when the first UDP attempt needs it.
    endpoint: Option<Endpoint>,
    params: Params,
    /// The tier that is tried now.
    current: usize,
    /// A connection that opened by itself and is to carry the session, with its tier.
    prepared: Option<(Link, usize)>,
    /// Failed attempts on the current tier since the last success.
    failures: usize,
    /// Whether a connection was ever made, so that the order of the tiers is settled.
    linked_once: bool,
    /// What the last failed attempt said, for the status message.
    note: Option<String>,
    io: Io,
    input: ReplayBuffer,
    /// Since when input is typed that the server has not acknowledged.
    input_since: Option<Instant>,
    output_offset: u64,
    /// Output that the server has been told about.
    acked_output: u64,
    echo: Echo,
    compress: bool,
    compressor: Compressor,
    stdin_open: bool,
    chunk: Vec<u8>,
    size: WindowSize,
    /// Whether a `Start` may have reached the server already.
    started: bool,
    net: Option<mpsc::UnboundedReceiver<Cause>>,
    tracker: Tracker,
    /// The message that should be on the screen, and the one that is.
    status_wanted: Option<String>,
    status_shown: Option<String>,
    /// Since when the status waits for the end of a sequence of the remote side.
    status_blocked_since: Option<Instant>,
}

/// Runs one session until it ends, and returns how it ended.
pub async fn run_session(params: Params, io: Io) -> Result<Finished, String> {
    if params.tiers.is_empty() {
        return Err("no way to reach the server".to_owned());
    }
    let size = io.size;
    let echo = Echo::new(params.local_echo, size.cols());
    let compress = params.compress;
    let mut io = io;
    let net = Some(std::mem::replace(&mut io.net, mpsc::unbounded_channel().1));
    Client {
        endpoint: None,
        params,
        current: 0,
        prepared: None,
        failures: 0,
        linked_once: false,
        note: None,
        io,
        net,
        tracker: Tracker::new(),
        status_wanted: None,
        status_shown: None,
        status_blocked_since: None,
        input: ReplayBuffer::new(INPUT_BUFFER),
        input_since: None,
        output_offset: 0,
        acked_output: 0,
        echo,
        compress,
        compressor: Compressor::new(),
        stdin_open: true,
        chunk: vec![0u8; 16 * 1024],
        size,
        started: false,
    }
    .run()
    .await
}

impl Client {
    async fn run(mut self) -> Result<Finished, String> {
        let mut attempts = 0usize;
        loop {
            let hello = if self.started {
                Hello::Resume {
                    size: self.size,
                    output_offset: self.output_offset,
                }
            } else if self.params.attach {
                Hello::Attach { size: self.size }
            } else {
                Hello::Start {
                    size: self.size,
                    env: self.params.env.clone(),
                    agent: self.params.agent.is_some(),
                }
            };
            if attempts > 0 && attempts.is_multiple_of(RESOLVE_EVERY) {
                self.refresh_address().await;
            }
            vlog!(
                2,
                "attempt {}: {} over {}",
                attempts + 1,
                hello_name(&hello),
                self.params.tiers[self.current].name()
            );
            match self.establish_watched(&hello).await {
                Attempt::Fatal(message) => {
                    vlog!(1, "cannot continue: {message}");
                    self.clear_status_at_end().await;
                    return Err(message);
                }
                Attempt::Linked(mut link) => {
                    if !self.linked_once
                        && let Some(Announce {
                            destination,
                            name,
                            started,
                        }) = &self.params.announce
                    {
                        // without the message of the day of a login, nothing else says when the
                        // session begins. The terminal is raw by now, hence the carriage return.
                        let what = if *started || matches!(hello, Hello::Start { .. }) {
                            "started session"
                        } else {
                            "attached to session"
                        };
                        let id = self.params.connect.id.as_str();
                        let session = name
                            .as_ref()
                            .map_or_else(|| id.to_owned(), |name| format!("{name} ({id})"));
                        let line = format!(
                            "[{what} {session} on {destination} over {}]\r\n",
                            self.params.tiers[self.current].name()
                        );
                        let _ = self.io.output.write_all(line.as_bytes()).await;
                        let _ = self.io.output.flush().await;
                    }
                    attempts = 0;
                    self.failures = 0;
                    self.linked_once = true;
                    self.started = true;
                    vlog!(
                        1,
                        "session running over {}",
                        self.params.tiers[self.current].name()
                    );
                    self.clear_status().await;
                    let pumped = self.pump(&mut link).await;
                    // what was shown ahead of the server goes, whatever happens next.
                    let taken_back = self.echo.cancel();
                    let _ = self.io.output.write_all(&taken_back).await;
                    let _ = self.io.output.flush().await;
                    match pumped? {
                        Pumped::Done(finished) => {
                            vlog!(1, "session ended: {finished:?}");
                            link.finish().await;
                            self.clear_status_at_end().await;
                            return Ok(finished);
                        }
                        Pumped::Lost => vlog!(1, "connection lost, reconnecting"),
                        Pumped::Switch(better, tier) => {
                            vlog!(1, "switching to {}", self.params.tiers[tier].name());
                            self.prepared = Some((better, tier));
                            continue;
                        }
                    }
                }
                // the answer was fine and only the kind of hello was wrong: no pause, no other
                // tier.
                Attempt::Again => continue,
                Attempt::Retry => {
                    if self.try_next_tier() {
                        continue;
                    }
                }
            }
            attempts += 1;
            vlog!(
                1,
                "attempt {attempts} failed{}",
                self.note
                    .as_ref()
                    .map_or_else(String::new, |note| format!(": {note}"))
            );
            let text = match self.note.take() {
                Some(note) => format!("connection lost, reconnecting (attempt {attempts}, {note})"),
                None => format!("connection lost, reconnecting (attempt {attempts})"),
            };
            self.show_status(&text).await;
            let pause = BACKOFF[attempts.saturating_sub(1).min(BACKOFF.len() - 1)];
            if let Some(finished) = self.wait_offline(pause).await? {
                self.clear_status_at_end().await;
                return Ok(finished);
            }
        }
    }

    async fn show_status(&mut self, text: &str) {
        self.status_wanted = Some(text.to_owned());
        self.sync_status(false).await;
    }

    async fn clear_status(&mut self) {
        self.status_wanted = None;
        self.sync_status(false).await;
    }

    /// Ends the display for good, even inside a sequence: nothing more will arrive to finish it.
    async fn clear_status_at_end(&mut self) {
        self.status_wanted = None;
        self.sync_status(true).await;
    }

    /// Writes the status change that is waiting, unless that would split a sequence of the remote
    /// side or a character. It is retried after every piece of output.
    async fn sync_status(&mut self, force: bool) {
        if self.status_wanted == self.status_shown {
            self.status_blocked_since = None;
            return;
        }
        if !force && self.tracker.in_sequence() {
            self.status_blocked_since.get_or_insert_with(Instant::now);
            return;
        }
        self.status_blocked_since = None;
        let bytes = match &self.status_wanted {
            Some(text) => self.io.status.show(text),
            None => self.io.status.clear(self.tracker.title()),
        };
        let _ = self.io.output.write_all(&bytes).await;
        let _ = self.io.output.flush().await;
        self.status_shown = self.status_wanted.clone();
    }

    /// Lets a waiting status through when the remote side stopped in the middle of a sequence for
    /// good. The terminal is given a CAN, which ends the sequence in every terminal, and the rest
    /// of it is dropped when it arrives. A sequence that is merely slow is not touched.
    async fn unstick_status(&mut self) {
        let waited = self
            .status_blocked_since
            .is_some_and(|since| since.elapsed() >= SEQUENCE_PATIENCE);
        if waited && self.tracker.in_sequence() {
            let _ = self.io.output.write_all(b"\x18").await;
            let _ = self.io.output.flush().await;
            self.tracker.cancel_sequence();
            self.sync_status(false).await;
        }
    }

    /// Writes the notice for `dropped` forgotten bytes into the output.
    async fn note_gap(&mut self, dropped: u64) {
        let _ = self.io.output.write_all(&dropped_notice(dropped)).await;
        let _ = self.io.output.flush().await;
        // the rest of an open sequence is gone, so the tracker must not wait for it.
        self.tracker.abandon_sequence();
        self.sync_status(false).await;
    }

    /// Takes typed input into the buffer while there is no connection.
    ///
    /// Returns when the pause is over, or earlier if the user leaves with the escape.
    async fn wait_offline(&mut self, pause: Duration) -> Result<Option<Finished>, String> {
        let pause = sleep(pause);
        tokio::pin!(pause);
        let mut tick = interval(PING_EVERY);
        loop {
            let room = self.input.free().saturating_sub(1).min(self.chunk.len());
            tokio::select! {
                _ = &mut pause => return Ok(None),
                _ = tick.tick() => self.unstick_status().await,
                read = self.io.input.read(&mut self.chunk[..room]), if self.stdin_open && room > 0 => {
                    match read {
                        Ok(n) if n > 0 => {
                            let typed = self.chunk[..n].to_vec();
                            if let Some(finished) = self.handle_typed(&typed, None).await? {
                                return Ok(Some(finished));
                            }
                        }
                        _ => self.stdin_open = false,
                    }
                }
                Some(size) = self.io.resizes.recv() => self.size = size,
                Some(()) = self.io.hangups.recv() => {
                    vlog!(1, "told to end while the connection is down: the session keeps running");
                    return Ok(Some(Finished::HungUp));
                }
                cause = next_cause(&mut self.net) => match cause {
                    // the network is different now, so waiting longer makes no sense.
                    Some(_) => {
                        self.rebind();
                        return Ok(None);
                    }
                    None => self.net = None,
                },
            }
        }
    }

    /// The frame for typed bytes: compressed when the uplink is what holds the input back (a
    /// paste over a slow link), plain otherwise. Typing is a few bytes and never compressed.
    /// Tells the server how much output was shown: at once when it is a lot, or when `force` says
    /// that a frame goes out anyway, and otherwise not yet.
    ///
    /// An acknowledgement alone is a small packet that nobody answers. On the TCP transport the
    /// next small packet behind it, such as a keystroke, can wait for the acknowledgement timer of
    /// the other side, which runs for 40 to 100 ms (see `bench/wprobe.py`). Sent in the write of
    /// the keystroke, it costs nothing.
    fn ack_output(&mut self, out: &mpsc::UnboundedSender<ClientFrame>, force: bool) {
        let unacked = self.output_offset.saturating_sub(self.acked_output);
        if unacked > 0 && (force || unacked >= ACK_BYTES) {
            let _ = out.send(ClientFrame::OutputAck(self.output_offset));
            self.acked_output = self.output_offset;
        }
    }

    fn input_frame(&mut self, bytes: &[u8]) -> ClientFrame {
        let waiting = self.input.end() - self.input.start();
        if self.compress
            && waiting >= COMPRESS_BACKLOG
            && bytes.len() >= COMPRESS_MIN_CHUNK
            && let Some(packed) = self.compressor.compress(bytes)
        {
            vlog!(
                2,
                "compressed {} bytes of input to {}",
                bytes.len(),
                packed.len()
            );
            return ClientFrame::Compressed(packed);
        }
        ClientFrame::Input(bytes.to_vec())
    }

    /// Applies typed bytes: input for the shell, up to the detach key, which leaves the session.
    async fn handle_typed(
        &mut self,
        typed: &[u8],
        link: Option<&mpsc::UnboundedSender<ClientFrame>>,
    ) -> Result<Option<Finished>, String> {
        let key_at = self
            .params
            .detach_key
            .and_then(|key| typed.iter().position(|byte| *byte == key));
        let bytes = &typed[..key_at.unwrap_or(typed.len())];
        if !bytes.is_empty() {
            let stored = self.input.push(bytes);
            self.input_since.get_or_insert_with(Instant::now);
            if stored < bytes.len() {
                // the reads leave room for what is free, so this does not happen.
                return Err("the input buffer overflowed".to_owned());
            }
            if let Some(link) = link {
                // the acknowledgement of the output goes out in the same write.
                self.ack_output(link, true);
                let frame = self.input_frame(bytes);
                let _ = link.send(frame);
                // only with a connection: without one the echo cannot come.
                let drawn = self.echo.typed(bytes, Instant::now());
                if !drawn.is_empty() {
                    let _ = self.io.output.write_all(&drawn).await;
                    let _ = self.io.output.flush().await;
                }
            }
        }
        if key_at.is_none() {
            return Ok(None);
        }
        if let Some(link) = link {
            let _ = link.send(ClientFrame::Detach);
        }
        Ok(Some(Finished::Detached))
    }

    /// Passes bytes between the terminal and the server until the session ends or the link breaks.
    async fn pump(&mut self, link: &mut Link) -> Result<Pumped, String> {
        let mut ping = interval(PING_EVERY);
        let mut stalled = false;
        // a change of the network that is waiting for the notifications to stop.
        let mut change: Option<(Cause, tokio::time::Instant)> = None;
        // when the server has to have answered after the client moved to a new path.
        let mut probe: Option<tokio::time::Instant> = None;
        // when the probe went out, and how many notices the change brought, for the debug output.
        let mut probed_at: Option<Instant> = None;
        let mut notices = 0usize;
        let mut resizes = Debounce::new(RESIZE_QUIET);
        // QUIC tried again on a new network while the session runs over a stream.
        let mut upgrade: Option<Upgrade> = None;
        // a second connection over the same stream transport, opened while the stream is silent.
        let mut spares: tokio::task::JoinSet<
            Result<Result<Link, String>, tokio::time::error::Elapsed>,
        > = tokio::task::JoinSet::new();
        let mut spare_not_before = Instant::now();
        // when to make the program draw again, after debug lines were written over the screen.
        let mut redraw_at: Option<tokio::time::Instant> = None;
        // the channels to the agent live as long as the link.
        let mut agent = self
            .params
            .agent
            .clone()
            .map(|socket| Forward::new(socket, link.out.clone()));
        loop {
            let room = self.input.free().min(self.chunk.len());
            let change_at = change.map(|(_, at)| at);
            if crate::log::take_terminal_lines() {
                redraw_at = Some(tokio::time::Instant::now() + REDRAW_AFTER_LOG);
            }
            let echo_due = self.echo.due();
            tokio::select! {
                read = self.io.input.read(&mut self.chunk[..room]), if self.stdin_open && room > 0 => {
                    match read {
                        Ok(n) if n > 0 => {
                            let typed = self.chunk[..n].to_vec();
                            if let Some(finished) = self.handle_typed(&typed, Some(&link.out)).await? {
                                return Ok(Pumped::Done(finished));
                            }
                        }
                        // end of input: the shell keeps running, like with `ssh`.
                        _ => self.stdin_open = false,
                    }
                }
                frame = link.reader.next() => {
                    let Some(frame) = frame? else {
                        vlog!(1, "the server closed the connection");
                        return Ok(Pumped::Lost);
                    };
                    link.last_heard = Instant::now();
                    if probe.take().is_some()
                        && let Some(sent) = probed_at.take()
                    {
                        vlog!(1, "the server answered on the new network after {:?}: the connection stays", sent.elapsed());
                    }
                    if std::mem::take(&mut stalled) {
                        self.clear_status().await;
                    }
                    vlog!(3, "<- {}", server_frame_name(&frame));
                    // compressed output is the same as plain output once it is inflated.
                    let frame = match frame {
                        ServerFrame::Compressed(packed) => ServerFrame::Output(
                            inflate(&packed)
                                .map_err(|e| format!("the server sent bad compressed output: {e:?}"))?,
                        ),
                        other => other,
                    };
                    match frame {
                        ServerFrame::Output(mut data) => {
                            // frames that are here already go to the terminal in one write. A
                            // terminal hands out its output in small pieces, and one write (a
                            // trip through the blocking pool) for each would cap the throughput.
                            while data.len() < OUTPUT_BATCH && link.reader.take_ready_output(&mut data) {}
                            // the rest of a sequence that was cancelled on the terminal is dropped.
                            let skipped = self.tracker.skip_prefix(&data);
                            let shown = &data[skipped..];
                            let (before, after) = self.echo.output(shown, Instant::now());
                            let written = async {
                                self.io.output.write_all(&before).await?;
                                self.io.output.write_all(shown).await?;
                                self.io.output.write_all(&after).await?;
                                self.io.output.flush().await
                            };
                            if written.await.is_err() {
                                return Err("cannot write to the terminal".to_owned());
                            }
                            self.tracker.feed(shown);
                            // a status that was held back because of a sequence can go out now.
                            self.sync_status(false).await;
                            self.output_offset += data.len() as u64;
                            if !link.reader.frame_ready() {
                                self.ack_output(&link.out, false);
                            }
                        }
                        ServerFrame::InputAck(offset) => {
                            self.input
                                .ack(offset)
                                .map_err(|_| "the server acknowledged input that was never sent".to_owned())?;
                            self.input_since = (self.input.end() > self.input.start()).then(Instant::now);
                        }
                        ServerFrame::Pong => link.ping_since = None,
                        ServerFrame::AgentOpen(id) => match agent.as_mut() {
                            Some(agent) => agent.open(id).await,
                            // nothing was offered, so a server that opens a channel is wrong.
                            None => link.send(ClientFrame::AgentClose(id)),
                        },
                        ServerFrame::Agent(id, data) => {
                            if let Some(agent) = agent.as_mut() {
                                agent.data(id, &data);
                            }
                        }
                        ServerFrame::AgentClose(id) => {
                            if let Some(agent) = agent.as_mut() {
                                agent.close(id);
                            }
                        }
                        ServerFrame::Gap(skipped) => {
                            // the server skipped output that this client was too slow to take.
                            // What was shown ahead of the server goes first, then the notice.
                            vlog!(1, "the server skipped {skipped} bytes of output (slow link)");
                            let taken_back = self.echo.cancel();
                            let _ = self.io.output.write_all(&taken_back).await;
                            self.output_offset = self
                                .output_offset
                                .checked_add(skipped)
                                .ok_or("the server skipped more output than there can be")?;
                            self.note_gap(skipped).await;
                            self.ack_output(&link.out, true);
                        }
                        ServerFrame::Detached => {
                            vlog!(1, "`ssj detach` was run for this session: leaving it");
                            return Ok(Pumped::Done(Finished::Detached));
                        }
                        ServerFrame::Exit(status) => {
                            link.send(ClientFrame::Close);
                            return Ok(Pumped::Done(Finished::Exited(exit_code(status))));
                        }
                        ServerFrame::Error(code) => return Err(format!("the server ended the session: {code:?}")),
                        ServerFrame::Welcome(_) => return Err("the server sent a second welcome".to_owned()),
                        // turned into output above.
                        ServerFrame::Compressed(_) => unreachable!("inflated above"),
                    }
                }
                Some(()) = self.io.hangups.recv() => {
                    if self.params.hangup_detaches {
                        // the connection of an `ssh` session ended, which is a loss and not a wish.
                        vlog!(1, "the terminal was closed inside an ssh session: leaving the session running");
                        link.send(ClientFrame::Detach);
                        return Ok(Pumped::Done(Finished::Detached));
                    }
                    vlog!(1, "the terminal was closed or ssj was told to end: ending the session");
                    link.send(ClientFrame::Hangup);
                    return Ok(Pumped::Done(Finished::HungUp));
                }
                Some(size) = self.io.resizes.recv() => {
                    vlog!(2, "the terminal is now {}x{}", size.cols(), size.rows());
                    self.size = size;
                    self.echo.resize(size.cols());
                    resizes.push(size, tokio::time::Instant::now());
                }
                _ = async { tokio::time::sleep_until(tokio::time::Instant::from_std(echo_due.expect("guarded"))).await }, if echo_due.is_some() => {
                    // nobody echoed what was shown, as with a password prompt.
                    let taken_back = self.echo.expire(Instant::now());
                    let _ = self.io.output.write_all(&taken_back).await;
                    let _ = self.io.output.flush().await;
                }
                _ = async { tokio::time::sleep_until(resizes.due().expect("guarded")).await }, if resizes.due().is_some() => {
                    if let Some(size) = resizes.take(tokio::time::Instant::now()) {
                        link.send(ClientFrame::Resize(size));
                    }
                }
                cause = next_cause(&mut self.net) => match cause {
                    Some(cause) => {
                        notices += 1;
                        vlog!(2, "network change notice {notices}: {cause:?}");
                        let strongest = change.map_or(cause, |(old, _)| old.max(cause));
                        change = Some((strongest, tokio::time::Instant::now() + CHANGE_QUIET));
                    }
                    None => self.net = None,
                },
                _ = async { tokio::time::sleep_until(change_at.expect("guarded")).await }, if change_at.is_some() => {
                    let (cause, _) = change.take().expect("guarded");
                    let transport = self.params.tiers[self.current].name();
                    vlog!(
                        1,
                        "network change ({cause:?}, {notices} notices): {}",
                        if link.quic.is_some() {
                            "moving the UDP socket and probing the server".to_owned()
                        } else {
                            format!("the {transport} stream has no socket to move, probing the server")
                        }
                    );
                    notices = 0;
                    self.rebind();
                    // a faster network may have appeared: QUIC is tried on it while the stream
                    // works.
                    if self.current != 0
                        && upgrade.is_none()
                        && matches!(self.params.tiers[0], Tier::Udp { .. })
                    {
                        vlog!(1, "trying QUIC on the new network");
                        let attempt = self.prepare_link(0);
                        upgrade = Some(Box::pin(timeout(UPGRADE_TIMEOUT, attempt)));
                    }
                    link.ping();
                    probed_at = Some(Instant::now());
                    let wait = if cause == Cause::Woke { PROBE_AFTER_WAKE } else { PROBE_AFTER_CHANGE };
                    probe = Some(tokio::time::Instant::now() + wait);
                }
                _ = async { tokio::time::sleep_until(probe.expect("guarded")).await }, if probe.is_some() => {
                    // the new path did not bring an answer, so the old connection is not worth
                    // keeping.
                    vlog!(1, "no answer after the network change: giving up the connection and reconnecting");
                    return Ok(Pumped::Lost);
                }
                result = async { upgrade.as_mut().expect("guarded").await }, if upgrade.is_some() => {
                    upgrade = None;
                    match result {
                        Ok(Ok(better)) => {
                            vlog!(1, "QUIC works on the new network");
                            return Ok(Pumped::Switch(better, 0));
                        }
                        Ok(Err(error)) => vlog!(1, "QUIC does not work on the new network: {error}"),
                        Err(_) => vlog!(
                            1,
                            "QUIC has no answer on the new network: staying on {}",
                            self.params.tiers[self.current].name()
                        ),
                    }
                }
                Some(joined) = spares.join_next(), if !spares.is_empty() => {
                    match joined {
                        Ok(Ok(Ok(better))) => {
                            // the stream may have come back while the spare was opening, and then
                            // it is the better one: nothing is in flight on it that a move
                            // would have to repeat.
                            if self.lag(link) < SPARE_AFTER / 2 {
                                vlog!(1, "a spare connection is open, but the stream answers again: keeping the stream");
                                spares.abort_all();
                            } else {
                                vlog!(1, "a spare connection is open: moving to it");
                                return Ok(Pumped::Switch(better, self.current));
                            }
                        }
                        Ok(Ok(Err(error))) => {
                            vlog!(1, "a spare connection failed: {error}");
                            spare_not_before = Instant::now() + SPARE_PAUSE;
                        }
                        Ok(Err(_)) => vlog!(1, "a spare connection did not open in time"),
                        Err(_) => {}
                    }
                }
                () = async { tokio::time::sleep_until(redraw_at.expect("guarded")).await }, if redraw_at.is_some() => {
                    redraw_at = None;
                    redraw(self.size, &link.out).await;
                }
                _ = ping.tick() => {
                    // A frame that arrived a moment ago shows that the connection works, so a ping
                    // is for the quiet times. It also takes the acknowledgement of the output with
                    // it, so that none goes out alone.
                    if link.last_heard.elapsed() >= PING_EVERY / 2 {
                        self.ack_output(&link.out, true);
                        link.ping();
                    }
                    self.unstick_status().await;
                    let silent = link.last_heard.elapsed();
                    // the server may be heard while it hears nothing: a ping or typed input
                    // without an answer is as bad as silence.
                    let lag = self.lag(link);
                    let stall_after = link
                        .quic
                        .as_ref()
                        .map_or(STALL_MIN, |conn| (conn.rtt() * 4).clamp(STALL_MIN, STALL_MAX));
                    // QUIC finds a returning network by itself and survives long outages. A
                    // stream has no way to notice that its path is gone, and the `ssh` behind it
                    // would hang, so a long silence ends it and a new one is made.
                    // not while a spare is opening, which would lose its progress, but not for ever:
                    // a transport that never opens has to give way to the next one.
                    let patience = if !spares.is_empty() {
                        self.params.stream_dead_after * 3
                    } else {
                        self.params.stream_dead_after
                    };
                    if link.quic.is_none() && lag > patience {
                        vlog!(1, "no answer for {lag:?} on a stream: giving it up");
                        return Ok(Pumped::Lost);
                    }
                    if link.quic.is_none() {
                        if lag > SPARE_AFTER
                            && spares.len() < SPARES_AT_ONCE
                            && upgrade.is_none()
                            && Instant::now() >= spare_not_before
                        {
                            vlog!(
                                1,
                                "no answer for {lag:?} on a stream: opening a spare connection ({} open)",
                                spares.len()
                            );
                            let attempt = self.prepare_link(self.current);
                            spares.spawn(timeout(SPARE_TIMEOUT, attempt));
                        } else if lag < SPARE_AFTER / 2 && !spares.is_empty() {
                            vlog!(1, "the stream answers again: dropping the spare connections");
                            spares.abort_all();
                            while spares.try_join_next().is_some() {}
                        }
                    }
                    if silent > stall_after {
                        stalled = true;
                        // the number changes in steps of five seconds, so the title does not
                        // flicker.
                        let text = if silent < Duration::from_secs(10) {
                            "no response from the server".to_owned()
                        } else {
                            format!("no response from the server for {}s", silent.as_secs() / 5 * 5)
                        };
                        self.show_status(&text).await;
                    }
                }
            }
        }
    }
}
