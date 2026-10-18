# *s*ession *s*urvives *j*umps

We want a secure shell (ssh) alternative with the following properties.

## Requirements

1. Non-failure as in `mosh`: the session survives roaming, IP changes, sleep, and packet loss.
1. Full scroll mode as in eternal terminal (`et`): the native scrollback of the local terminal
   works, so the stream is forwarded as bytes and not as synchronized screen state.
1. No root on the remote server: no privileged port, no daemon, no setuid binary.
1. UDP transport for speed.
1. Written in memory safe Rust. The workspace lint `unsafe_code` is `deny`, so `unsafe` needs an
   explicit `#[allow(unsafe_code)]` with a safety comment. There is one such place:
   `daemon::close_inherited_descriptors`, which calls `rustix::io::close`. Dependencies such as
   `ring`, `rustix`, and `tokio` contain `unsafe` code, which is audited through the checks listed
   under Security and not avoided.
1. No new security vulnerabilities: no home-made cryptography, and no weaker trust model than `ssh`.

## Architecture

Workspace layout follows the guidelines (`crates/`, edition 2024, shared `Cargo.toml` values in
the workspace root):

- `crates/ssj-proto`: wire format, framing, and session state machine (no IO).
- `crates/ssj-net`: QUIC transport shared by client and server: pinned mutual TLS, address
  validation (retry), and the transport limits.
- `crates/ssj-server`: runs on the remote host as the unprivileged user, owns the session.
  The shell lives behind the `Backend` and `Shell` traits (`backend` module), so where it runs
  can change without touching the session logic. v1 has one backend, `Direct`, which runs the
  shell as a child of the server on a PTY (via `pty-process`).
- `crates/ssj-client`: local terminal, bootstrap, and reconnect logic.
- `crates/ssj-holder` (optional, later): the holder process of the `holder` backend described
  below. It is a separate binary with no network code.

### Bootstrap

1. Client generates an ephemeral keypair and self-signed certificate.
1. Client runs `ssh user@host <server path>` (see Installation of the server; existing `ssh`
   handles authentication, host keys, and jump hosts) and sends the fingerprint of its certificate on stdin. Stdin is used because argv
   is visible to other users in `ps`.
1. Server generates its own ephemeral keypair and certificate, binds a UDP port from a high
   range and a TCP port on the loopback address, prints the session id, both ports, and the server
   certificate fingerprint on stdout (`SSJ CONNECT <id> <udp:PORT|none> <tcp:PORT|none> <fingerprint>`), and detaches from the `ssh` channel. The range defaults to 60000 to 61000 and is set with `--port-range LOW:HIGH`, which
   the client also accepts and passes on to `ssj-server`. `--port-range 0` takes any port that the
   kernel gives, for hosts where the range is full or filtered anyway.
1. Client reads the line, closes the `ssh` connection, and connects over the best transport (see
   Transports).

No bearer secret exists. Both fingerprints are public values that need integrity, not
confidentiality, and the authenticated `ssh` channel provides that. The server expires the
pinned client fingerprint if no client connects within a short timeout.

### Transport

Decided: QUIC via `quinn` and `rustls` with mutual TLS. Each side accepts only the certificate
whose fingerprint was exchanged during bootstrap, so a host that can reach the UDP port but did
not take part in the bootstrap cannot open a session.

- Provides UDP, TLS 1.3, loss recovery, congestion control, and connection migration, so none of
  these are implemented by us.
- The idle timeout is disabled or very long, and the application layer adds keepalives.
- If the QUIC connection dies, the client reconnects with the session ID and resumes the stream.

Rejected alternative: custom reliable stream over UDP with Noise (`snow`). More control, but more
code to audit.

Findings of the M0 spike, whose code and tests now live in `crates/ssj-net`:

- Pinning in a custom `rustls` verifier works for both directions. A peer presenting the pinned
  certificate with a different private key is rejected by the handshake signature check.
- With the idle timeout disabled and a 1 second keepalive, one connection survived a 90 second
  total network blackout and delivered its pending data 0.4 seconds after the network returned.
  Reconnecting is therefore the fallback and not the normal path.
- A client that changes its source address keeps the same connection (QUIC migration).
- A client that lost its connection without a close can be replaced: the server closes the old
  connection when a newer authenticated one arrives.

### Session persistence

- The server process outlives the connection. It keeps the PTY and its child shell running.
- The server buffers output not yet acknowledged by the client, bounded in size (`replay_capacity`,
  default 4 MiB). On reconnect, the client sends the last received offset and the server replays
  from there. If the server had to forget output in the meantime, see Output while nobody listens.
- Client input is also numbered and acknowledged, so no keystroke is lost or duplicated.
- The server exits when the shell exits or after a configurable detached timeout (default: no
  timeout for the shell, on for the pinned client fingerprint before the first connect).

### Protocol version

There is no backward compatibility, which keeps the code small. Every binary speaks exactly one
protocol version, and two peers talk only if their versions are equal.

- The protocol version is an integer `N` with a single source, the constant `PROTOCOL` in
  `ssj-proto`. It appears in the ALPN identifier `ssj/N` (the only one a peer offers or accepts)
  and in the name of the server file (see Installation of the server).
- `N` is increased whenever the wire format or its meaning changes in a way that an older peer
  would not understand: a changed or removed frame, a changed field, a new frame, a new
  handshake step. Changes that do not touch the wire format (client behavior, terminal handling,
  messages) leave `N` alone.
- A golden test keeps this honest: it encodes every frame variant and compares the bytes with the
  stored file `tests/golden/protocol-<N>.bin`. A change of the wire format makes the test fail
  until the developer increases `PROTOCOL` and stores a new golden file. Increasing `PROTOCOL`
  without a new file fails the same test, because the file is missing.
- `PROTOCOLS.md` is the table of releases: one row per release with its version and protocol.
  It is checked by the release workflow, which fails if the version being released is missing or
  its row differs from `PROTOCOL`. The client uses the table only for messages. When a handshake
  fails with `no_application_protocol`, the client says which release speaks its protocol, for
  example "client 0.4.0 speaks protocol 3, install it on the host with `ssj install`".
- The bootstrap line `SSJ CONNECT` has no version of its own and is covered by `N`.

### Reconnect (client)

QUIC keeps one connection alive through long outages, so most disconnects need no action by the
client. A new connection is made only when the old one ends (the server closed it, or the path
was lost for good). Then:

1. The client shows the status `connection lost, reconnecting (attempt N)` and tries again after
   0.25, 0.5, 1, 2, and then every 3 seconds. One attempt lasts at most 8 seconds.
1. Each attempt sends `Resume` with the output offset the client has processed. The server answers
   with the input offset it has consumed. The client drops that input from its buffer and sends
   the rest again, so nothing is lost and nothing is repeated, including keystrokes typed during
   the outage. They wait in the bounded input buffer.
1. If the server forgot output while the client was away, the answer starts at the oldest byte that
   is left, the client prints a notice with the exact number of bytes that it missed, and goes on.
   If the session is gone, or the server claims the client has seen bytes that never existed
   (`ReplayGap`), the client stops with a clear message.
1. If the first `Start` may not have arrived, the client tries `Resume` first and falls back to
   `Start` when the server answers `NotStarted`.
1. While there is no connection, `~.` still works, so the user is never stuck. The server is not
   told, and the session keeps running with its timeout.

The client sends a `Ping` every second. It reports a stall when no frame has come for four round
trips of the connection, but not sooner than 2 seconds and not later than 10, so a good link
reports quickly and a slow link does not report all the time. The message is `no response from the server`, with the time added in steps of five seconds after ten seconds, and it is removed with
the next frame.

Network changes and wakes from sleep:

- A source sends a cause on one channel: `NetworkChanged` or `Woke` (stronger). Sources are a clock
  watch that runs everywhere (a timer that fires 3 seconds late means that the process was
  suspended, judged on whichever of the wall clock and the monotonic clock saw more time pass),
  the dynamic store of SystemConfiguration on macOS, and a netlink socket for link, address, and
  route changes on Linux. The platform sources sit behind the cargo feature `netwatch`, on by
  default. A source that fails to start only makes the reaction slower.
- Notifications are collected until they stop for 300 milliseconds, since a change of network
  produces many. Then the client moves its UDP socket to a new one (`Endpoint::rebind`), so QUIC
  probes the new path at once and the server follows the new address, and sends a `Ping`. If
  nothing comes back within 2 seconds (1 second after a wake), the old connection is dropped and a
  new one is made with `Resume`.
- A cause during the pause between attempts ends the pause. A cause during an attempt that hangs
  ends that attempt and starts a new one on the new socket.
- No source can be tested without a real change of the network. The clock watch, the collecting,
  and the reaction are tested with injected causes. The macOS source starts in a test, and
  `cargo nextest run -p ssj-client --test netwatch --run-ignored only` waits for a real change. The Linux source
  has been compiled but not run. `cargo run -p ssj-client --example netwatch` prints what the
  sources report.

After every third failed attempt the client looks up the address of the server again (2 seconds at
most), so that a changed address is found after a move or a long sleep. A failed lookup keeps the
old address. If the address family changes, the UDP socket is replaced by one of the new family.

Window sizes are collected: the client sends a size when no newer one has come for 50 milliseconds
(trailing edge), and the server applies only the newest size of the messages that are queued, so
dragging a window edge does not send a signal to the shell for every step.

Status messages go to the window title. The client follows the output of the server just far
enough to know whether it stopped inside an escape sequence or inside a multi-byte character, and
holds the message back until it did not. A sequence that is still open two seconds after the
message became due (the link stalled in the middle of it) is cancelled: the client writes CAN to
the terminal, which ends the sequence in every terminal, shows the message, and drops the rest of
that sequence from the output when it arrives (up to its final byte, or the last byte of the
character). Nothing else is changed or dropped, and a half title is never taken for the title.
When the message goes,
the title stack of xterm is popped and the last title that the remote side set (read from OSC 0
and OSC 2 in the output) is set by name, since not every terminal has the stack. At the end of the
session the message is removed even inside a sequence. Without a terminal, the messages go to
stderr as lines.

### Transports

UDP can be blocked, and a host behind a jump host cannot be reached by datagrams at all. The
session core works on any pair of byte streams, so three transports carry the same frames, in
this order of preference:

1. **UDP**: QUIC, as described above. Fastest, and the only one that survives a changing path
   without a new connection.
1. **TCP through `ssh -W`**: the server also listens on `127.0.0.1` (kernel assigned port, only
   reachable from the host itself). The client runs `ssh -W 127.0.0.1:<port> user@host` and speaks
   TLS 1.3 through it, with the same pinned mutual authentication, no resumption, and no 0-RTT.
   `TCP_NODELAY` is set on the server side. The jump host chain of the `ssh` configuration works.
1. **The `ssh` channel**: `ssj-server --stdio <id>` connects to the control socket, sends
   `STREAM`, and after the answer `OK` the socket is the byte stream of a client. The process
   copies it to stdout and from stdin. Shell startup files may print text first, so the stream
   begins after the 17 bytes `\0SSJ_SYNC_STREAM\n`, and the client skips everything before them.
   No TLS runs inside, since `ssh` already authenticates and encrypts, and the socket allows only
   the same user.

The choice is `--transport auto|udp|tcp|stdio` or `transport` in `client.toml`. `auto` tries UDP
for 1.5 seconds in the first connection, then TCP, then the `ssh` channel, and each failure moves
on at once. It leaves out UDP when `ssh -G` shows a `proxyjump` or `proxycommand` for the host.
After a connection was made, its transport is kept for three failed attempts before the next one
is tried, since the network is the likelier culprit. The server offers what it is told to
(`--no-udp`, `--no-tcp`); the `ssh` channel is always there.

A stream has no keepalive of QUIC: if the path dies without a sign (a firewall, a hung `ssh`),
the connection stays open. The client pings every second, shows "no response" after 2 seconds,
and gives a stream up after 10 seconds of silence. Then it makes a new one and resumes with the
offsets, as after any loss. Every connection on these two transports starts a new `ssh`, which
cannot ask for a passphrase (`BatchMode=yes`), so they need an agent or a shared connection
(`ControlMaster auto`, `ControlPersist`) when the key has a passphrase. The same holds for the
reconnects of a session that started over UDP and later fell back.

### Destination, address, and names

The destination is `[user@]host[:port]` or `ssh://[user@]host[:port]`, with IPv6 addresses in
brackets (`[2001:db8::1]:22`) or bare (`2001:db8::1`, without a port). The text goes to `ssh` and
to DNS, so only the characters of names and addresses are accepted (letters, digits, `.`, `_`,
`-`, and for the user `+`), and never a leading `-` or `.`. The client gives `ssh` the user and
host, and the port as `-p`.

The address of the server for UDP is, in this order: `--udp-host`; the `HostName` that `ssh -G`
reports for the destination (the destination may be an alias from the `ssh` configuration);
the host part of the destination. DNS is asked again during reconnects, see above. `ssh -G` also
shows a jump host or proxy command, which makes `auto` leave out UDP.

A session may have a name (`ssj --name work user@host`): 1 to 32 characters of letters, digits,
`.`, `_`, `-`, not starting with `.` or `-`. The server keeps it and `ssj list` shows it. Names are
unique per user and host. The command joins the session of that name if it runs and starts one
otherwise; the client always begins with an attach, and a session without a shell answers
`NotStarted`, on which the client sends a start. `ssj attach` and `ssj kill` take an id or a name.
`auto_attach = true` in `client.toml` (or `--auto-attach`) makes a plain `ssj user@host` join the
only session that has no client connected, and start a new one when there is none or more than one
(a session with a client is never taken away). The names and the state travel in the line `SSJ SESSION <id> <state> <age> <protocol> <name|->` of the control socket, which is not part of the
protocol between client and server.

### Command line, completions, and release files

- The client parses its command line with `clap` (derive). The definition lives in
  `crates/ssj-client/src/options.rs`, free of other dependencies of the crate, and every value is
  text there; `cli::parse` checks the values. A word is a command (`list`, `attach`, `kill`,
  `install`) only when a destination follows it, so a host named `list` still works. An option
  given twice takes the last value, except `--server-arg` and `--send-env`, which collect.
- `build.rs` writes the man page (`ssj.1`, with a FILES section for `client.toml` and
  `server.toml`) and the completions for Bash, Zsh, and Fish to `$OUT_DIR`, and passes the path to
  the compiler as `SSJ_ASSETS`. The options for internal use (`--complete`, `--batch`) are left
  out.
- `ssj list --json` prints a JSON array of `{"id", "name", "state", "age_secs", "protocol"}`.
  The name is `null` for an unnamed session.
- Completion of session ids and names reads a cache, never the network. Every `ssj list` writes
  `$XDG_CACHE_HOME/ssj/sessions/<destination>.list` (default `~/.cache`): a first line
  `@<destination>`, an optional `@port=<n>`, then one id or name per line. The directory is mode
  0700, the file is replaced by rename. `ssj --complete [DESTINATION]` prints the names of one host
  or of all, and starts `ssj --batch list <destination>` in the background for each list older
  than 10 seconds. `--batch` adds `-o BatchMode=yes -o ConnectTimeout=2` to the `ssh` command, so
  that a refresh never asks for a passphrase or a key, and takes a lock file next to the list so
  that one refresh per host runs at a time (a lock older than 30 seconds is stale). Lines read
  back from the cache are checked again: a name must be a valid selector, and the destination must
  parse as one without a port, since it reaches `ssh` later.
- The id or name comes before the destination (`ssj attach ID host`), so the completion cannot know
  the host and offers the names of all hosts with a list.
- A release publishes one archive per client platform, `ssj-<version>-<target>.tar.gz`, that holds
  a directory with `ssj`, the server files `ssj-server-<target>` of all five builds (next to the
  client, where the installer looks), `ssj.1`, `completions/`, `README.md`, `LICENSE`, and a
  `SHA256SUMS` of the binaries. The release also lists the server files alone, a `SHA256SUMS` of
  everything, and the build provenance attestation for all of them.

### Benchmark

`bench/bench.py` (Python 3, standard library only) runs `ssh` and `ssj` over UDP, TCP, and the
`ssh` channel against one host, each client on a pseudo terminal as a user runs it:

- bulk output: `dd | tr` of 256 MiB by default, in MB/s, the best of three rounds;
- keystroke latency: one byte at a time into a raw `cat`, the median, 95th percentile, and maximum
  of the round trip;
- a paste of 8 MiB into `head -c`, in seconds.

`--netem "delay 40ms loss 1%"` wraps the run in `tc qdisc add dev lo root netem ...` (Linux, root),
which hits `ssh` and `ssj` alike when the host is `localhost`. `--server` picks the server file,
which is how the glibc and the musl build are compared. The workflow `bench.yml` runs both builds
with and without `netem` on a GitHub runner against its own `sshd`, by hand. `bench/linux-bench.sh`
runs the same inside a container (`docker run ... sh bench/linux-bench.sh`).

Bulk output with `--raw-bulk` (no `tr`, which limits every client on some systems), 256 MiB, the
best of three rounds; the paste is 8 MiB; the latency is the median of 150 to 200 samples:

| system | client | bulk MB/s | median ms | paste s |
| ------ | ------ | --------- | --------- | ------- |
| Linux aarch64, glibc, `sshd` in a container | ssh | 180 | 9.6 | 0.11 |
| | udp | 193 | 0.89 | 0.08 |
| | tcp | 174 | 1.11 | 0.08 |
| | stdio | 189 | 1.46 | 0.09 |
| macOS 26, Apple silicon, `localhost` | ssh | 84 | 9.3 | 1.25 |
| | udp | 74 | 0.58 | 1.31 |
| | tcp | 62 | 0.64 | 1.74 |

Reading them:

- The latency of `ssh` is dominated by its keystroke timing obfuscation (OpenSSH 9.5 and later
  sends keystrokes in 20 ms ticks while typing), so the comparison there says little about the
  transport. `ssj` answers a key in under a millisecond.
- On Linux `ssj` over UDP is as fast as `ssh` for bulk output and faster for a paste.
- On macOS the pseudo terminal hands out its data in pieces of exactly 1024 bytes (Linux: up to
  4 KiB), so every kilobyte of output is one pass through the event loop of the server. The shell
  alone reaches 115 MB/s there, `ssh` 84, and `ssj` 74. The TCP tier is slower because its bytes
  also pass through an `ssh -W` process. The gap to `ssh` on macOS is about 12 percent.

What the measurements changed (each found with `sample`, CPU times from `ps`, and counters):

- Both programs use a single-threaded runtime. With several threads every wake-up for a small
  chunk of output crossed threads, and the server used two cores for 47 MB/s; it now uses less
  than one core for 74.
- The client writes all output frames that are already waiting to the terminal in one write
  (up to 256 KiB). Each write goes through the blocking pool of tokio, and with frames of 2 KiB
  that cost 4.8 s of a 5.9 s transfer. Before this, the throughput also depended on the size of
  the replay buffer, because acknowledgements were sent only when the reader was empty.
- The frame reader looks at a frame without decoding it (`peek_server`), reads straight into its
  buffer (with `read_buf`, since the future is dropped by `select!`: a read into a buffer padded
  with zeros would leave the zeros behind), and drops used bytes once per read.
- The replay buffer reserves its memory once and copies blocks and not bytes. The tracker skips
  plain text without stepping through it.
- The QUIC endpoints ask the peer to acknowledge every eighth packet (ACK frequency extension)
  and not every second one, which saved about 10 percent on macOS and nothing for latency.

Nothing is gated yet.

### Installation of the server

The server needs no root: it is one file in the home directory of the user.

Location and name:

- `$XDG_DATA_HOME/ssj/bin/ssj-server-p<N>` (default `~/.local/share/ssj/bin/`), where `N` is the
  protocol version, not the version of the binary. A host that serves clients of different
  protocol versions therefore has several files side by side, one per protocol, and each client
  uses its own.
- Installing a newer release with the same protocol replaces the file of that protocol by atomic
  rename, which does not disturb a running session, since the running process keeps its old file.
- The client starts the server by this absolute path and does not rely on `PATH`, because the
  `PATH` of a non-interactive `ssh` command often lacks `~/.local/bin`. `--server-path` overrides
  the location for servers installed elsewhere (for example by an administrator).
- A session stays on the protocol it was started with. After updating the client to a newer
  protocol, the old sessions are attached with an older client or ended. `ssj --list` shows the
  protocol of each session, and the attach error says which release to use.
- `ssj gc user@host` removes server files of protocols that have no running session, except the
  protocol of the client that runs the command.

Two ways to get it there:

1. Explicit: `ssj install user@host` uploads the matching binary.
1. On first use: when `ssj user@host` finds no server file for its protocol, the wrapper of the
   remote command reports `SSJ MISSING` with exit code 66, which the client tells apart from every
   other failure. What happens next follows the policy `auto_install`:
   - `ask` (default): on a terminal, the client asks `Install it now? [y/N]` and only `y` or `yes`
     counts. Without a terminal there is nobody to ask, so nothing is installed and the message
     names `ssj install` and `--install`.
   - `always`: installs without asking. `never`: never installs and says how to do it by hand.
   - The flags `--install` and `--no-install` override the config file for one run.
   - `attach`, `list`, and `kill` never install. Without a server there are no sessions.

How the upload works:

1. The client asks the host for its system (`uname -sm`, in a line of its own that must appear
   exactly once) and maps it to a build: Linux x86_64 and aarch64 get the static `musl` build,
   macOS arm64 gets the native one. Anything else is refused with a clear message.
1. The client takes the file `ssj-server-<build>` from `--server-dir` (default: the directory of
   the client itself, where a release archive puts it), or the file named by `--server-binary`.
1. It sends the SHA-256 digest of the file on the first line of stdin, followed by the file. The
   remote script writes to a temporary file (`umask 077`), computes the digest again with
   `sha256sum` or `shasum -a 256`, and compares. It then makes the file executable and runs
   `--version` on it, which catches `noexec` mounts. Only after both checks it moves the file into
   place by atomic rename. Any failure removes the temporary file.
1. The file name contains the protocol, so a present file is by definition the right one.

Trust in the server file:

- The digest that the client sends protects the transfer against damage and truncation. It does
  not protect against a swapped local file, since the client computes it from the same file that
  it uploads. The trust anchor is the file next to the client, which comes from the same release.
- A client could carry the digests of the server files of its release and refuse a different file.
  This was built and removed again: it needs the release pipeline to patch each client binary
  after the servers are built (and, on macOS, to sign it again), and it adds a crate and a tool for
  a protection that the checksums and the attestation of the release already give for the whole
  set of files.
- Checksums (`SHA256SUMS`) and a build provenance attestation are published with every release,
  see Supply chain. Verify the downloaded files against them before putting a server file next to
  the client.

Failure cases, each with a clear message:

- The directory is on a `noexec` mount. The client reports the directory and suggests
  `--server-path` or another `XDG_DATA_HOME`.
- The remote has no `sha256sum` or `shasum`. The install is refused, since it cannot be verified.
- No write permission or no space. The upload is deleted.
- The remote architecture is not in the list.

Security notes:

- Executing an uploaded file adds no privilege: it runs as the same user that `ssh` already gives
  a shell to.
- The digest check protects against corruption and a tampered download. It does not protect
  against a compromised release, so releases are published with checksums and artifact
  attestations (see Supply chain).
- The client never executes a downloaded binary locally and never installs anything on the local
  machine except through the normal installation of the client.

### Attach and restart

Cases by what restarts:

- Network change, sleep, or packet loss: the same connection or a reconnect continues the
  session, as described above.
- Client restart (crash, reboot, closed terminal): the session is still alive on the server, but
  the new client has a new ephemeral identity, so a plain reconnect fails the pinning check. The
  client re-attaches through `ssh`.
- Server process crash or host reboot: the shell is lost, since the PTY closes with the process.
  The only protection is to run the shell inside a multiplexer such as `tmux`. This is the same
  limit as in `mosh` and `et`.

Re-attach:

1. Each session has an ID and a control socket (mode `0600`, in the runtime directory). The
   server checks the peer credentials of every connection to the socket and accepts only its own
   uid.
1. The client runs `ssh user@host ssj-server --attach [ID]` and sends its new fingerprint on
   stdin, as in the normal bootstrap. Without an ID, it attaches to the only session, or fails
   and lists the sessions if there are several.
1. The `--attach` process connects to the control socket and asks the running server to replace
   the pinned client fingerprint. Authorization comes from `ssh`: only a process of the same user
   can reach the socket.
1. The server prints the `SSJ CONNECT` line again, with new ports and a new fingerprint. The old connection, if any, is
   closed with reason `replaced`.
1. After the handshake, the client sends its window size, so that full-screen programs redraw.
   The server replays the unacknowledged tail of the output buffer.

If the session has no shell yet, because its first client never connected, `Attach` is answered
with `NotStarted` and the client sends `Start` instead, which also delivers its environment.

When `Attach` replaces the endpoint, the old one is closed first. Its UDP port stays busy until
QUIC has drained the connections, so the server waits for that and retries a busy port for up to
two seconds. Without this a narrow port range would refuse an immediate re-attach.

The client keeps the session ID and host in `$XDG_STATE_HOME/ssj/` as a hint. The ID is not a
secret. `ssj --list user@host` shows the sessions of a host.

### Ending and leaving a session

Ending a session and leaving it are different actions, and only the first one ends the shell.

Ending (the session is over):

- The shell exits: `exit`, Ctrl-D on an empty line, or the shell dying from a signal. The server
  sends the exit status, the client prints nothing of its own and exits with the same code, and
  the server process ends. This is what a user expects from `ssh`. The server ends when the client
  confirms the status with `Close`. If the connection breaks without it, the status may not have
  arrived, so the session stays, sends the status again to the next connection, and ends when that
  client closes, or after `exit_grace` (60 seconds) without a client.
- `ssj kill [ID] user@host` ends a session from another terminal. The server sends `SIGHUP` to the
  process group of the shell, as a closing terminal would, and ends after the shell has exited.
- `SIGTERM` or `SIGINT` to the server process does the same as `ssj kill`.
- `SIGHUP` to the server process is ignored, because it is sent when the `ssh` channel of the
  bootstrap closes.

Leaving (the shell keeps running):

- Closing the terminal, or `SIGHUP`, `SIGTERM`, or `SIGINT` to the client, is an accidental or
  involuntary disconnect. The client just ends, and the session survives, because surviving
  is the purpose of the tool. The detached timeout applies.
- An escape key sequence detaches on purpose. As in `ssh`, the escape character `~` counts only
  at the start of a line (right after a newline or at the start of the session). Then:
  - `~.` detaches: the client sends `Detach`, prints `[detached from <ID>]`, and exits with code 0.
  - `~~` sends a literal `~`.
  - `~?` prints the list of escape sequences.
  - Any other character after `~` is sent as typed, including the `~`.
    The escape character is configurable (`--escape-char C` or `escape_char` in `client.toml`),
    and `none` turns the feature off.
- `Detach` keeps the session running and **disables the detached timeout** until a client
  attaches again. The reason is intent: a user who detaches on purpose wants to come back later,
  for example to a long job, so the server must not end the shell behind their back. After the
  next attach, losing the connection involuntarily brings the timeout back.
- On `Detach` the server also closes its UDP endpoint and drops the pinned client fingerprint.
  The key of the client exists only in the memory of the client process, which has exited, so
  the pin could never match again anyway. A detached session therefore listens on no port. The
  next attach goes through `ssh`, as described under Attach and restart, and opens a new port
  with a new pin.
- An involuntary disconnect is different: the client may still be alive and reconnect, so the
  endpoint stays open until the detached timeout ends the session.
- This is not a vulnerability in either case. Only a holder of the pinned key can open a session,
  and the open port of an involuntary disconnect is limited to handshake attempts, which are
  bounded by retry and by the limits listed under Hardening requirements. What a session without
  timeout costs is a live shell in the background, such as resources and forgotten jobs.
  `ssj list user@host` shows all sessions with their state and age, and the server config can set
  `max_detached` as a hard upper limit even for deliberate detaches (default: none).

Frames: `Detach` is a client frame of its own. `Close` means that the client is done, for
example after it received the exit status, and a `Close` before the exit is treated as a lost
connection.

### Surviving a server process restart (optional, later)

By default the shell is a child of `ssj-server` and ends with it. Two optional backends keep it
alive. Neither is part of v1.

Multiplexer backend (`--persist zellij` or `--persist tmux`):

- `ssj-server` starts the shell as `zellij attach --create ssj-<ID>` (or the `tmux` equivalent).
  The multiplexer has its own daemon, so the shell survives a crash or upgrade of `ssj-server`.
  Re-attach then reconnects to the multiplexer session.
- Zellij is used as an external program and not as a library. It is an application with its own
  client and server processes and no stable embedding interface.
- Cost: the multiplexer redraws the screen and keeps its own scrollback. This breaks the full
  scroll mode of the terminal (requirement 2), so the backend is opt-in and the UI of the
  multiplexer should be reduced to the minimum (no status bar, no pane frames).
- A host reboot still ends the session, since the multiplexer is gone too.

Holder backend (`--persist holder`), added as a second implementation of `Backend` with
`SURVIVES_SERVER_RESTART` set to true and a working `attach`:

- A small separate process `ssj-holder` owns the PTY and the shell. It does nothing except pass
  bytes between the PTY and a Unix socket, with no screen emulation, so native scrollback is
  kept.
- `ssj-server` connects to the holder as a client. A crash or upgrade of the network-facing
  server leaves the holder and the shell running, and the new server reconnects to it.
- The holder has no network code and a very small attack surface. It also keeps the replay
  buffer, so output produced while no server runs is not lost.
- Same limit: a host reboot ends the session.

Optional, later: on a binary upgrade the server re-executes itself and passes the PTY file
descriptor to the new process. This is a cheaper variant of the holder, for upgrades only and
not for crashes. Rebuilding a PTY from a file descriptor needs `unsafe` in `pty-process`
(`Pty::from_fd`), which `deny(unsafe_code)` rules out in our crates without a reason. The holder backend does
not have this problem, so it is preferred.

### Session core (server)

One task owns the state of a session: the shell, the output buffer, and the current connection.
Connections arrive as byte streams, so the logic does not depend on QUIC and is tested with
in-memory streams and real shells.

- A connection that sends nothing for 60 seconds, not even a ping, is given up like a lost one:
  the detached timeout starts, and the client resumes when it returns. QUIC alone would keep the
  connection of a vanished client for ever (no idle timeout), and the session would never count
  as detached.
- Reading the shell, writing to it, and talking to the client never wait for each other. The
  shell is split into reader, writer, and exit parts for this reason.
- The reader of a connection keeps parsing frames while the shell is busy. Acknowledgements of
  output travel in the same byte stream as the input and are what lets a chatty program
  continue, so holding them behind a blocked input frame is a deadlock (found by the paste
  test). Input is queued with a byte budget of 2 MiB per connection, which is above the 1 MiB
  that the client may leave unacknowledged. A client that sends more waits, and only it.
- While a client acknowledges its output, the shell is read only as far as the replay buffer has
  room, so a client that is slow cannot make the server buffer more than the capacity, and
  nothing is lost (backpressure as in `ssh`). Output while nobody listens works differently, see
  the next section.
- The client leaves in two ways (see Ending and leaving a session). `Detach` keeps the session
  and disables the detached timeout. A lost connection, or a `Close` before the exit, keeps the
  session with the timeout. The session ends when the shell has exited and the exit was queued for
  the client and the connection then ends. If the connection is lost right after the exit was
  queued but before it arrived, the client finds no server on its next attempt and reports the
  end of the session without the exit code.
- A new connection replaces the old one. Frames queued for the old one get five seconds to leave.

### Output while nobody listens

A session exists so that a long job (a build, a training run) goes on while the client is away.
Backpressure would defeat that: after one buffer full of output the shell would block in its
`write` until a client acknowledged something. So the server forgets the oldest output instead
whenever nobody is there to acknowledge it, which is the case when

- no client is connected (lost connection or deliberate detach), or
- a client is connected but has acknowledged nothing for `away_after` (10 seconds by default, set in `server.toml` or with `--away-after`) while the buffer
  stayed full. A connection can die without the server hearing of it (a laptop that sleeps), so
  silence with a full buffer counts as away.

Rules:

- The reading of the PTY never stops while nobody listens. The newest `replay_capacity` bytes are
  kept, and the start of the buffer moves forward by exactly the number of bytes that were lost.
- A connection that is still owed bytes that were forgotten is closed, since it cannot be served
  anymore. Its client resumes on a new connection.
- `Resume` with an offset before the start of the buffer is not an error. The `Welcome` carries the
  start of the buffer as its output offset, and the offset may only move forward. The client takes
  it over, and the difference is the number of bytes it missed.
- The client writes the notice into the output at that place: a CAN byte (it cancels an escape
  sequence the terminal may be in the middle of, since the rest of it was lost), the reset of
  colors and styles (`ESC [ 0 m`), and the line `[ssj: N bytes of output were dropped]`, with N
  exact. The notice comes from the client and not from the server, because bytes that the server
  inserted into the stream would shift the offsets of both sides.
- `Attach` is not affected: it replays everything the server still holds (the buffer is
  `replay_capacity` large, and output that a client acknowledged is gone), starting at a character
  boundary. The client begins the replay with CAN and the reset of colors, since it starts in the
  middle of the stream.
- The output of a job that nobody read is lost for good, as in `tmux` with a limited history. Setting
  `replay_capacity` higher keeps more of it, at the price of memory on the host.
- A `Resume` offset beyond the end of the output is still refused with `ReplayGap`.
- On macOS, the terminal driver can write a carriage return twice (`\r\r\n`) when the reader is
  slower than the writer. This is a property of that system and happens without `ssj` too, so the
  tests of exact byte streams turn the output processing of the terminal off (`stty -opost`).

### Environment forwarding

Works like `SendEnv` and `AcceptEnv` in `ssh`: the client selects variables, the server filters
them.

- Client: `--send-env PATTERN` (repeatable, shell-style `*` and `?`) selects variables from the
  local environment. The default is `TERM`, `LANG`, and `LC_*`.
- Server: the accepted names come from the server config file (`server.toml` in the config
  directory below, key `accept_env`), with the same default. They are never taken from the command line that the
  client passes through `ssh`, since that would let the client override the filter.
- The variables travel in the session start frame inside the encrypted QUIC connection, never in
  the `ssh` bootstrap command.
- They are applied once, when the shell is created. After a reconnect the client does not resend
  them, because the running shell cannot change its environment.
- Validation on the server: names match `[A-Za-z_][A-Za-z0-9_]*`, values contain no NUL byte,
  and the limits are 64 variables, 256 bytes per name, 8 KiB per value, and 64 KiB in total. A
  violation rejects the whole set and the session start fails with a clear message.
- Loader and startup variables (`LD_*`, `DYLD_*`, `BASH_ENV`, `ENV`, `PROMPT_COMMAND`, and
  `SSH_*`) are never matched by a wildcard. They are accepted only when named exactly in
  `accept_env`.

The filter is protection against mistakes and unexpected variables, not a privilege boundary.
A client with the pinned key already has a full shell as the same user.

### Files and paths

Paths follow the XDG Base Directory specification, on Linux and on macOS alike.

- Config: `$XDG_CONFIG_HOME/ssj/` (default `~/.config/ssj/`), holding `server.toml` and
  `client.toml`.
- State: `$XDG_STATE_HOME/ssj/` (default `~/.local/state/ssj/`), holding the optional log file.
- A variable with a relative path is invalid and ignored, as the specification requires.
- Runtime: `$XDG_RUNTIME_DIR/ssj/` holds one control socket per session. macOS has no
  `XDG_RUNTIME_DIR`, so the fallback is `$XDG_STATE_HOME/ssj/run/<host>/`. The host name is in the
  path because a home directory can be shared by several hosts, and a socket of another host
  would look dead and be removed. A path that is too long for a socket (about 100 bytes) moves to
  `/tmp/ssj-<uid>/`. The directory must be owned by the user, must not be a symbolic link, and
  must not be accessible to others, or the server refuses to use it. The socket contains no
  secret, and it is removed when the session ends.
- Nothing is written to the cache directory. Session keys stay in memory.
- Created directories get mode `0700` and files mode `0600`. The server refuses to read a config
  file that is writable by group or others.

### Scrollback

Output bytes are written unchanged to the local terminal. No screen emulation on the client, so
scrollback, selection, and search of the terminal emulator work. The cost is that local echo
prediction as in `mosh` is out of scope.

## Security

- Authentication relies on the `ssh` bootstrap. No password or key handling in `ssj`. Every `ssh`
  the client starts gets `-e none`, so that a `~` in the data it sends is never an escape.
- Each session has its own ephemeral certificates on both sides. Nothing is stored on disk.
- No secret is printed or logged. Only certificate fingerprints cross the `ssh` channel.
- The server accepts one client per session and rejects every handshake that does not present
  the pinned client certificate. QUIC still answers unauthenticated packets (Initial packets and
  version negotiation), so the UDP port of a session is detectable by a scan. This reveals no
  secret, since the fingerprint is public, and the exposure is limited by the rules below.
- Replay and tampering protection come from the transport (TLS 1.3 / AEAD).
- Resource limits: bounded replay buffer, bounded packet sizes, and bounded handshake rate.
- Pinning is implemented in a custom `rustls` certificate verifier that compares the fingerprint
  and still verifies the handshake signature. A fingerprint comparison alone would let anyone who
  has seen the public certificate impersonate that side.
- TLS session resumption and 0-RTT are disabled, since 0-RTT data can be replayed.
- Dependencies are kept minimal and checked with `cargo audit` and `cargo deny` in CI.
- Fuzz the frame parser in `ssj-proto`.

## Hardening requirements

Network edge (server):

- Address validation by stateless retry for every new handshake, which removes spoofed-source
  floods and the amplification of the first flight (`Incoming::retry` in `quinn`). The cost is one
  round trip on the first connect only.
- Bounded handshake load: low `max_incoming` and incoming buffer sizes, and `refuse` or `ignore`
  for sources that fail handshakes repeatedly.
- Bounded streams and windows: explicit limits for concurrent streams, stream receive windows, and
  datagram size, set so that a peer cannot make the server buffer more than the replay buffer.
- ALPN `ssj/1` is required on both sides, so no other TLS or QUIC client can complete a handshake.
- One live connection per session. A newer authenticated connection replaces the older one.
- Large pastes: the initial congestion window is 256 KiB, so slow start does not cost several
  round trips. Measured at 100 ms round trip on localhost: 64 KiB in 0.08 s, 1 MiB in 0.34 s, and
  8 MiB in 1.2 s. The receive window of the server is 1 MiB per stream and 2 MiB per connection.

Input path (client):

- The client cuts input, such as a paste, into frames of at most 32 KiB. This is a courtesy to the
  stream and not the safety measure, since the server enforces the limits again on every frame.
- The unacknowledged input is kept in a bounded buffer (1 MiB by default). When it is full, the
  client stops reading from the terminal until the server acknowledges input, so a huge paste
  waits in the terminal and not in memory.
- Backpressure is chained: a full PTY blocks the server, which stops reading the stream, which
  fills the window of QUIC, which blocks the client.

Session and shell (server):

- The shell is the login shell of the user from the password database. The client never selects
  the command, the environment, or the working directory.
- The shell starts with a clean environment. `SSH_*` and `XDG_SESSION_*` variables inherited
  from the bootstrap session are removed, because they describe a connection that no longer
  exists. The only variables the client can add are the ones allowed by the rules under
  Environment forwarding.
- Window size, frame lengths, and offsets are range checked. An offset outside the retained
  replay buffer closes the connection.
- While a client is connected and acknowledging, a full replay buffer stops the reading from the
  PTY (backpressure as in `ssh`) and no output is dropped. While nobody listens, the oldest output
  is forgotten instead (see Output while nobody listens), so that a detached job is never frozen.
- The server disables core dumps (`RLIMIT_CORE` 0). Private keys are not zeroed: both sides need
  their key for every reconnect until the session ends, and the copy inside the TLS library
  cannot be reached.
- Detach redirects stdio to `/dev/null`, so the `ssh` channel closes and its output cannot leak.
  The server ignores `SIGHUP` and calls `setsid`. Both server processes (the one `ssh` starts and
  the daemon) close every descriptor above 2 first thing, from the list in `/proc/self/fd` or
  `/dev/fd`, or by counting up to the limit of open files (at most 65536) when there is no list.
  This is the only place with `unsafe` in the workspace, and it is sound because nothing owns a descriptor
  yet. So neither the daemon nor the shell holds a socket or file of an unusual parent.
- Log output goes to stderr only until detach, and afterwards nowhere unless a log file in the state
  directory is configured. Terminal content is never logged.
- Port selection: random free port from the range, retried on conflict. On shared hosts the range
  is shared by all users, so exhaustion produces a clear error.

Bootstrap (client):

- The client starts `ssh` with an argument vector and never through a shell. A host argument that
  starts with `-` is rejected, to prevent option injection such as `-oProxyCommand`.
- `ssh` runs without a terminal, so output is not altered and stdin is not echoed.
- The client accepts exactly one line with a fixed prefix, `SSJ CONNECT <id> <udp:PORT|none> <tcp:PORT|none> <fingerprint>`, of
  bounded length, and ignores other output such as banners and `.bashrc` noise. Remote text is
  never printed to the terminal unfiltered, because it could contain escape sequences.
- The server reads the client fingerprint from stdin with a length limit and a timeout.
- The status line of the client (such as "reconnecting") is drawn by the client itself and never
  contains text from the server.

Supply chain:

- `Cargo.lock` is committed, and CI runs `cargo audit` and `cargo deny` on every push and weekly.
  `deny.toml` limits licenses and registries and bans `openssl`, `native-tls`, and `aws-lc`, so the
  crypto provider stays `ring`. `cargo geiger` was tried to track `unsafe` in dependencies but
  fails to read the current lockfile, so new dependencies are reviewed by hand instead.
- Release binaries get checksums and GitHub artifact attestations, because they run on remote
  hosts.
- The protocol version is part of the ALPN string (see Protocol version). Peers with different
  versions refuse each other with a clear message.

Inherent to any remote shell and not mitigated: terminal escape sequences in the output of the
remote host (title changes, clipboard writes through OSC 52). This is the same exposure as `ssh`.

## Threat model

Bootstrap phase (over `ssh`):

- The trust is that of `ssh`. An attacker who defeats the `ssh` host key check can swap the
  fingerprints and sit in the middle of the UDP session as well. `ssj` adds no new weakness here
  and cannot fix a wrong host key.

UDP phase:

- An attacker on the path cannot impersonate either side, because each side accepts only the
  pinned certificate of the other. They cannot read, modify, or replay traffic.
- They can drop, delay, or reorder packets, or inject junk that is rejected. This is a denial of
  service, and the reconnect and replay logic recovers once the path clears.
- They can observe metadata: IP addresses, UDP port, packet sizes, and timing. Typing patterns
  can leak, as with `ssh`.
- An off-path attacker cannot hijack a roaming connection, because a new path must prove
  possession of the session keys.
- A local user on the remote host can occupy a UDP port before the server binds it. The client
  then fails the pin check, so the effect is a denial of service only.

## Platforms

- Client: macOS (arm64 only) and Linux.
- Server: macOS (arm64 only) and Linux.
- Linux targets: glibc and musl, each on x86_64 and arm64 (four release binaries).
- macOS target: arm64 only (one release binary).
- Release binaries are cross-compiled with `cargo-zigbuild` (Linux targets) and built natively
  on macOS. Each target ships `ssj` and `ssj-server`.
- Automatic installs use the static `musl` build, because it runs on every distribution. The
  `glibc` builds are published for packages and for those who want them. Performance is expected
  to be the same: the work is network I/O and encryption, and `ring` brings its own assembly for
  the ciphers, so the C library hardly matters. This is to be measured, not assumed: CI runs the
  paste and throughput tests on both builds, and `glibc` becomes the default only if it is
  clearly faster (more than 10 percent) in a test on a real link.
- Server code uses only portable POSIX calls (via `rustix`), so no Linux-only APIs such as
  `epoll` or `/proc` outside of optional, feature-gated code.

## Out of scope for v1

- Port forwarding, agent forwarding, and X11 forwarding.
- `sftp` or `scp` replacements.
- Local echo prediction.
- Windows client and server.

## Decisions

- The detached timeout has no default. A lost client leaves a live shell behind, and `ssj list`
  and `ssj kill` are the tools to find and end it. A user who wants a limit sets
  `detached_timeout`.
- `ssj-server` runs as root with a warning on stderr (root is never needed, but refusing helps
  nobody who has a reason).
- The client CLI uses `clap`. The options live in one file that the build script includes, so the
  man page and the completions cannot drift from the parser.
- One protocol version per binary, with no backward compatibility. A handshake error names the
  release to use, and old server files stay side by side (`ssj-server-p<N>`). Revisit at the first
  protocol bump.
