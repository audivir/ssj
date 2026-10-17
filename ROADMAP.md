# Roadmap

Everything that is decided but not built yet. Finished work is documented in `SPEC.md` and
`README.md` and is not listed here. An entry is removed when it is implemented and tested.

Order: the bug fixes first (2 and 3), then the features (4 and 5), then packaging, performance, and
the release (6 to 8).

## Decisions to record in `SPEC.md`

These were decided and still need to be written into the spec when their phase lands.

- One protocol version per binary, no backward compatibility. A handshake error names the release
  to use, and old server files stay side by side (`ssj-server-p<N>`). Revisit at the first protocol
  bump.
- The detached timeout has no default (`none`). `max_detached` stays an optional hard limit.
- Root is allowed, with a warning on stderr and in the log. `--allow-root` goes away.
- The UDP port range stays configurable, default 60000 to 61000. `port_range = "0"` means any port.
- The client CLI uses `clap`.
- Wake and network change use platform APIs behind one trigger, with the clock check as a fallback.
  (Built. The spec describes it already.)

## 2. Client robustness

- Resolve the host again during the reconnect loop, on every third attempt, so that a changed
  address is found after a move or a long sleep.
- Debounce window resizes: 50 ms trailing edge in the client, and the server applies only the last
  size of a burst.
- Pass `-e none` to every `ssh` the client starts.

## 3. Server hardening

- Control socket path: when `$XDG_RUNTIME_DIR/ssj/<id>.sock` would exceed the limit of `sun_path`
  (104 bytes on macOS, 108 on Linux), use `/tmp/ssj-<uid>/` with the same mode and owner checks.
- Home directories that several hosts share: record the host in the socket directory and do not
  list or remove sockets of other hosts.
- Check the values of `TERM`, `LANG`, and `LC_*` before they reach the shell: characters
  `A-Za-z0-9._@-` only, no path separators, length limit. Reject the whole set on a violation.
- Root: warn instead of refusing. Remove `--allow-root`. Default `detached_timeout` to `none`.
- `port_range = "0"` for any port, and an error message that suggests it when the range is full.
- Close all inherited file descriptors above 2 in the daemon, as `SPEC.md` already promises. Today
  they leak into the daemon and its shell (found in a test: a pipe of the caller stayed open). This
  needs more than `rustix` offers without `unsafe`, so it needs a decision first.
- The 10 second `away_after` for a silent client is a constant. Make it a setting.
- Zero the private keys after the handshake. Say in the docs what this does not cover.

## 4. Transport tiers

For jump hosts, blocked UDP, and hosts behind NAT. The session core stays unchanged, since it works
on any pair of byte streams.

1. One stream type in `ssj-net` for QUIC streams and for TLS over TCP.
1. Bootstrap line v2: `SSJ CONNECT <udp|none> tcp:<port> <fingerprint>`, with a strict parser and a
   fuzz target. The server also binds TCP on `127.0.0.1:0` (kernel assigned). `--no-udp` and
   `--no-tcp`.
1. Tier 1, TCP over `ssh -e none -W localhost:<port>`, mutual TLS 1.3 with the same pinned
   verifier, `TCP_NODELAY`, and the application ping deadline so that a half-open socket is noticed
   within seconds.
1. Tier 2, `ssj-server --stdio`, for hosts that forbid forwarding. It attaches to the running daemon
   through the control socket. It starts with a 16-byte sync header, `\0SSJ_SYNC_STREAM\n`, and the
   client skips everything before it (output of shell profiles).
1. Selection: `--transport auto|udp|tcp|stdio`. `auto` goes straight to Tier 1 when `ssh -G`
   shows a jump host, otherwise tries UDP for 1.5 seconds, then Tier 1, then Tier 2 when forwarding
   is refused.
1. Tests with a fake `ssh` that relays `-W`: jump host, dropped datagrams, forbidden forwarding,
   a banner before the sync header, and reconnect on every tier.

## 5. Names, destinations, and `ssh -G`

- Look up `hostname`, `user`, `port`, `proxyjump`, and `proxycommand` with `ssh -G`. This fixes the
  UDP address for aliases and replaces `--udp-host` as the default.
- `--name` for `ssj`, stored by the server (no change of the wire protocol), shown by `ssj list`, and
  accepted by `attach` and `kill`. Names are unique per user.
- `ssj --name work host` attaches when the session exists and creates it otherwise.
- `auto_attach = true` in `client.toml`.
- Destination parser for `user@host:port`, `ssh://`, and IPv6 literals, with the check against a
  leading `-` kept. Fuzz it.
- `ssj attach` without an id when exactly one session exists (done); with several, list them.

## 6. CLI and packaging

- Move the client CLI to `clap` (derive), keeping every flag and test. Generate completions for
  Bash, Zsh, and Fish and the man page in `build.rs`.
- `ssj list --json`, and a cache of the session list per host for instant tab completion, refreshed
  in the background with a single-flight lock and `-o BatchMode=yes -o ConnectTimeout=2` so that
  it never prompts for a passphrase or a security key.
- Release archives: one `.tar.gz` per client platform with the client, the server builds for all
  host systems (the installer looks for them next to the client), the man page, completions,
  `README.md`, `LICENSE`, and `SHA256SUMS`. Keep the build attestation.
- Optional: `ssj install --download`, fetching the server file from the release of the same version
  and checking it against `SHA256SUMS`.

## 7. Performance

- Benchmark harness (an ignored test and a CI job) that compares `ssh`, `ssj` over UDP, and `ssj`
  over TCP: bulk output, keystroke latency, and the 8 MiB paste under `tc netem` (Linux only).
- Run it on the glibc and musl builds and choose the default build for automatic installs from the
  numbers.
- First run without gates to see real numbers. Then gate on them (the proposal suggested 5 percent
  for throughput and 1 ms for latency, which nobody has measured).

## 8. Release

- Push the repository to a remote so that `ci.yml`, `fuzz.yml`, and `supply-chain.yml` run. They
  have never run, and the Linux tests have never been executed anywhere. Expect to fix things.
- Manual checks that no CI machine can do: the macOS and Linux network watch (switch Wi-Fi, sleep
  the laptop), listed in `cargo run -p ssj-client --example netwatch`.
- `PROTOCOLS.md` row, golden file, a tagged release, and a first run of the release workflow.
- Bring `SPEC.md` and `README.md` in line with the code, and remove statements that became false.

## Later, optional

- **Speculative local echo**, off by default (`local_echo = "off" | "auto"`). Predict only printable
  characters on a plain shell line, drawn dim, and confirm against the server output. Epoch rule of
  mosh: after Enter, show nothing until the server has echoed one character, which closes the race
  with password prompts. The server reports `ECHO` and canonical mode from `tcgetattr` as an extra
  signal. Prediction stops at the first escape sequence in the output, which covers full-screen
  programs. Needs a new server frame, so it lands before the first release or waits for protocol 2.
- **SSH agent forwarding** over a dedicated stream. Large and security sensitive.

## Known limits that stay until someone fixes them

- If the connection is lost right after the exit status was queued but before it arrived, the
  client finds no server and reports the end of the session without the exit code.
- `attach` replays only the last 64 KiB of output.
- A status message waits while the output stops inside an escape sequence, so a link that stalls in
  the middle of one shows no message until it recovers.
- The Linux network watch (netlink) has been compiled but not run.

## Considered and not planned

- Offering two protocol versions in the ALPN list: against the decision to keep the code small.
- Power-state APIs for sleep and wake: they need a run loop or D-Bus for no gain, since waking
  always brings the interfaces back and fires the network watch.
- Extra OSCs sent by `ssj` itself (desktop notification, progress indicator, badge): kept out for now.
- A server-side terminal emulator or `smcup` tracking: against the dumb-pipe rule. Full-screen
  programs are covered by the escape rule above.
- Hole punching for hosts behind NAT: the TCP tier through `ssh` covers them.
- A manifest of server digests inside the client: built and removed again as too complex for what
  the release checksums and attestation already give.
