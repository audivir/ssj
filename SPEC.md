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
- `crates/ssj-server`: runs on the remote host as the unprivileged user, owns the PTY.
- `crates/ssj-client`: local terminal, bootstrap, and reconnect logic.

### Bootstrap

1. Client generates an ephemeral keypair and self-signed certificate.
2. Client runs `ssh user@host ssj-server` (existing `ssh` handles authentication, host keys, and
   jump hosts) and sends the fingerprint of its certificate on stdin. Stdin is used because argv
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

Findings of the M0 spike (`crates/ssj-spike`, removed after M1 absorbs the code):

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
- Nothing is written to the runtime directory or to the cache directory. Session keys stay in
  memory.
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

- `Cargo.lock` is committed, and CI runs `cargo audit`, `cargo deny`, and a `cargo geiger` report
  to track `unsafe` in dependencies. The crypto provider is `ring`.
- Release binaries get checksums and GitHub artifact attestations, because they run on remote
  hosts.
- The protocol version is part of the ALPN string. A server and client of different versions
  refuse each other with a clear message.

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
- Server code uses only portable POSIX calls (via `rustix`), so no Linux-only APIs such as
  `epoll` or `/proc` outside of optional, feature-gated code.

## Out of scope for v1

- Port forwarding, agent forwarding, and X11 forwarding.
- `sftp` or `scp` replacements.
- Local echo prediction.
- Windows client and server.

## Open questions

1. Default for the detached timeout. With no timeout, a detached server keeps a live shell behind
   an open UDP port indefinitely. A finite default (for example 7 days) is safer.
2. Should `ssj-server` refuse to run as root (uid 0), since root is never needed?
