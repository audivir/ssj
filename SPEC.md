# *s*ession *s*urvives *j*umps

We want a secure shell (ssh) alternative with the following properties.

## Requirements

1. Non-failure as in `mosh`: the session survives roaming, IP changes, sleep, and packet loss.
2. Full scroll mode as in eternal terminal (`et`): the native scrollback of the local terminal
   works, so the stream is forwarded as bytes and not as synchronized screen state.
3. No root on the remote server: no privileged port, no daemon, no setuid binary.
4. UDP transport for speed.
5. Written in memory safe Rust, with `#![forbid(unsafe_code)]` in every crate of the workspace.
   Dependencies such as `ring`, `rustix`, and `tokio` contain `unsafe` code, which is audited
   through the checks listed under Security and not avoided.
6. No new security vulnerabilities: no home-made cryptography, and no weaker trust model than `ssh`.

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
2. Client runs `ssh user@host <server path>` (see Installation of the server; existing `ssh`
   handles authentication, host keys, and jump hosts) and sends the fingerprint of its certificate on stdin. Stdin is used because argv
   is visible to other users in `ps`.
3. Server generates its own ephemeral keypair and certificate, binds a UDP port from a high
   range, prints port and server certificate fingerprint on stdout, and detaches from the `ssh`
   channel. The range defaults to 60000 to 61000 and is set with `--port-range LOW:HIGH`, which
   the client also accepts and passes on to `ssj-server`.
4. Client reads the line, closes the `ssh` connection, and switches to UDP.

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
- The server buffers output not yet acknowledged by the client, bounded in size. On reconnect,
  the client sends the last received offset and the server replays from there.
- Client input is also numbered and acknowledged, so no keystroke is lost or duplicated.
- The server exits when the shell exits or after a configurable detached timeout (default off
  for the shell, on for the pinned client fingerprint before the first connect).

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
2. Each attempt sends `Resume` with the output offset the client has processed. The server answers
   with the input offset it has consumed. The client drops that input from its buffer and sends
   the rest again, so nothing is lost and nothing is repeated, including keystrokes typed during
   the outage. They wait in the bounded input buffer.
3. If the server no longer has the output (`ReplayGap`) or the session is gone, the client stops
   with a clear message.
4. If the first `Start` may not have arrived, the client tries `Resume` first and falls back to
   `Start` when the server answers `NotStarted`.
5. While there is no connection, `~.` still works, so the user is never stuck. The server is not
   told, and the session keeps running with its timeout.

The client sends a `Ping` every two seconds. After five seconds without any frame from the
server, it shows `no response from the server for Ns` and removes the message with the next frame.

Status messages go to the window title, with the title stack of xterm (`CSI 22;0t` and
`CSI 23;0t`) to restore the old title. The client has no copy of the screen, so it must not draw
over the application. A known limit: a message written in the middle of an escape sequence of the
remote side, which the output stream may be split inside, would corrupt that sequence. Without a
terminal, the messages go to stderr as lines.

### Address of the UDP connection

The client needs the address of the server for the UDP connection. Its default is the host part
of the destination, resolved by DNS. This is wrong when the destination is an alias from the
`ssh` configuration or when `ssh` goes through a jump host. `--udp-host` overrides it. A later
version should read the `HostName` that `ssh -G` reports. A jump host does not help for UDP,
because the datagrams do not travel through it.

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

1. Explicit (v1): `ssj install user@host` uploads the matching binary. This is the recommended
   default, since it is one visible step.
2. Automatic, on first use (optional, after v1): when the server file of the protocol of the client
   is missing, the client offers to install it, and with `--auto-install` it does so without asking.
   It never installs silently by default.

How the upload works:

1. The client runs `ssh user@host uname -sm` (no shell startup output is trusted, the answer is
   matched against a fixed list) and picks the target: Linux x86_64 or arm64, or macOS arm64.
   Linux targets use the static `musl` build, which runs on any distribution regardless of the
   `glibc` version. Other answers abort with a clear message.
2. The binaries for all five targets are known to the client by SHA-256 digest through a manifest
   that is compiled into the client. The binaries themselves are either bundled with the client
   or downloaded from the GitHub release of the same version, and the digest is checked before
   anything is sent.
3. The client streams the file over `ssh` into a temporary file in the target directory (created
   with `umask 077`), and the remote side runs `sha256sum` (or `shasum -a 256` on macOS) on it.
   Only if the digest matches the manifest, the file is made executable and renamed into place
   atomically. Otherwise it is deleted and the install fails.
4. The file name contains the protocol, so a present file is by definition the right one. The
   client does not compare binary versions. Whether the present file is current is irrelevant
   unless the user asks for an update with `ssj install --force`.

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
2. The client runs `ssh user@host ssj-server --attach [ID]` and sends its new fingerprint on
   stdin, as in the normal bootstrap. Without an ID, it attaches to the only session, or fails
   and lists the sessions if there are several.
3. The `--attach` process connects to the control socket and asks the running server to replace
   the pinned client fingerprint. Authorization comes from `ssh`: only a process of the same user
   can reach the socket.
4. The server prints `SSJ CONNECT <port> <fingerprint>` again. The old connection, if any, is
   closed with reason `replaced`.
5. After the handshake, the client sends its window size, so that full-screen programs redraw.
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
  the server process ends. This is what a user expects from `ssh`.
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
(`Pty::from_fd`), which `forbid(unsafe_code)` rules out in our crates. The holder backend does
not have this problem, so it is preferred.

### Session core (server)

One task owns the state of a session: the shell, the output buffer, and the current connection.
Connections arrive as byte streams, so the logic does not depend on QUIC and is tested with
in-memory streams and real shells.

- Reading the shell, writing to it, and talking to the client never wait for each other. The
  shell is split into reader, writer, and exit parts for this reason.
- The reader of a connection keeps parsing frames while the shell is busy. Acknowledgements of
  output travel in the same byte stream as the input and are what lets a chatty program
  continue, so holding them behind a blocked input frame is a deadlock (found by the paste
  test). Input is queued with a byte budget of 2 MiB per connection, which is above the 1 MiB
  that the client may leave unacknowledged. A client that sends more waits, and only it.
- Output is read from the shell only while the replay buffer has room, so a client that does not
  acknowledge cannot make the server buffer more than the capacity.
- The client leaves in two ways (see Ending and leaving a session). `Detach` keeps the session
  and disables the detached timeout. A lost connection, or a `Close` before the exit, keeps the
  session with the timeout. The session ends when the shell has exited and the exit was queued for
  the client and the connection then ends. If the connection is lost right after the exit was
  queued but before it arrived, the client finds no server on its next attempt and reports the
  end of the session without the exit code.
- A new connection replaces the old one. Frames queued for the old one get five seconds to leave.

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
  `XDG_RUNTIME_DIR`, so the fallback is `$XDG_STATE_HOME/ssj/run/`. The socket contains no
  secret, and it is removed when the session ends.
- Nothing is written to the cache directory. Session keys stay in memory.
- Created directories get mode `0700` and files mode `0600`. The server refuses to read a config
  file that is writable by group or others.

### Scrollback

Output bytes are written unchanged to the local terminal. No screen emulation on the client, so
scrollback, selection, and search of the terminal emulator work. The cost is that local echo
prediction as in `mosh` is out of scope.

## Security

- Authentication relies on the `ssh` bootstrap. No password or key handling in `ssj`.
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
- When the replay buffer is full, the server stops reading from the PTY (backpressure as in
  `ssh`) and never drops output.
- The server disables core dumps (`RLIMIT_CORE` 0) and zeroizes private keys after use.
- Detach closes all inherited file descriptors and redirects stdio to `/dev/null`, so the `ssh`
  channel closes and its output cannot leak. The server ignores `SIGHUP` and calls `setsid`.
- Log output goes to stderr only until detach, and afterwards nowhere unless a log file in the state
  directory is configured. Terminal content is never logged.
- Port selection: random free port from the range, retried on conflict. On shared hosts the range
  is shared by all users, so exhaustion produces a clear error.

Bootstrap (client):

- The client starts `ssh` with an argument vector and never through a shell. A host argument that
  starts with `-` is rejected, to prevent option injection such as `-oProxyCommand`.
- `ssh` runs without a terminal, so output is not altered and stdin is not echoed.
- The client accepts exactly one line with a fixed prefix, `SSJ CONNECT <port> <fingerprint>`, of
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

## Open questions

1. Default for the detached timeout, which applies to involuntary disconnects only. With no
   timeout, a lost client leaves a live shell behind an open UDP port indefinitely. A finite
   default (for example 7 days) is safer.
2. Should `ssj-server` refuse to run as root (uid 0), since root is never needed?
