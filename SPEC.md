# *s*ession *s*urvives *j*umps

We want a secure shell (ssh) alternative with the following properties.

## Requirements

1. Non-failure as in `mosh`: the session survives roaming, IP changes, sleep, and packet loss.
2. Full scroll mode as in eternal terminal (`et`): the native scrollback of the local terminal
   works, so the stream is forwarded as bytes and not as synchronized screen state.
3. No root on the remote server: no privileged port, no daemon, no setuid binary.
4. UDP transport for speed.
5. Written in memory safe Rust, with `#![forbid(unsafe_code)]` in every crate of the workspace.
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

### Session persistence

- The server process outlives the connection. It keeps the PTY and its child shell running.
- The server buffers output not yet acknowledged by the client, bounded in size. On reconnect,
  the client sends the last received offset and the server replays from there.
- Client input is also numbered and acknowledged, so no keystroke is lost or duplicated.
- The server exits when the shell exits or after a configurable detached timeout (default off
  for the shell, on for the pinned client fingerprint before the first connect).

### Scrollback

Output bytes are written unchanged to the local terminal. No screen emulation on the client, so
scrollback, selection, and search of the terminal emulator work. The cost is that local echo
prediction as in `mosh` is out of scope.

## Security

- Authentication relies on the `ssh` bootstrap. No password or key handling in `ssj`.
- Each session has its own ephemeral certificates on both sides. Nothing is stored on disk.
- No secret is printed or logged. Only certificate fingerprints cross the `ssh` channel.
- The server accepts one client per session and rejects everything that does not present the
  pinned client certificate, with no response to unauthenticated packets (no amplification, no
  port scan signal).
- Replay and tampering protection come from the transport (TLS 1.3 / AEAD).
- Resource limits: bounded replay buffer, bounded packet sizes, and bounded handshake rate.
- Pinning is implemented in a custom `rustls` certificate verifier that compares the fingerprint
  and still verifies the handshake signature. A fingerprint comparison alone would let anyone who
  has seen the public certificate impersonate that side.
- TLS session resumption and 0-RTT are disabled, since 0-RTT data can be replayed.
- Dependencies are kept minimal and checked with `cargo audit` and `cargo deny` in CI.
- Fuzz the frame parser in `ssj-proto`.

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
- Server code uses only portable POSIX calls (via `rustix`), so no Linux-only APIs such as
  `epoll` or `/proc` outside of optional, feature-gated code.

## Out of scope for v1

- Port forwarding, agent forwarding, and X11 forwarding.
- `sftp` or `scp` replacements.
- Local echo prediction.
- Windows client and server.
