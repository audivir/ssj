# Roadmap

Everything that is decided but not built yet. Finished work is documented in `SPEC.md` and
`README.md` and is not listed here. An entry is removed when it is implemented and tested.

Order: packaging first (2), then performance (3), and the release (4).

## Decisions to record in `SPEC.md`

These were decided and still need to be written into the spec when their phase lands.

- One protocol version per binary, no backward compatibility. A handshake error names the release
  to use, and old server files stay side by side (`ssj-server-p<N>`). Revisit at the first protocol
  bump.
- The client CLI uses `clap`.

## 2. CLI and packaging

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

## 3. Performance

- Benchmark harness (an ignored test and a CI job) that compares `ssh`, `ssj` over UDP, and `ssj`
  over TCP: bulk output, keystroke latency, and the 8 MiB paste under `tc netem` (Linux only).
- Run it on the glibc and musl builds and choose the default build for automatic installs from the
  numbers.
- First run without gates to see real numbers. Then gate on them (the proposal suggested 5 percent
  for throughput and 1 ms for latency, which nobody has measured).

## 4. Release

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

- Descriptors above 2 that a parent leaves open stay open in the daemon and the shell. Closing them
  needs `unsafe`.
- If the connection is lost right after the exit status was queued but before it arrived, the
  client finds no server and reports the end of the session without the exit code.
- `attach` replays only the last 64 KiB of output.
- A status message waits while the output stops inside an escape sequence, so a link that stalls in
  the middle of one shows no message until it recovers.
- The Linux network watch (netlink) has been compiled but not run.

## Considered and not planned

- Zeroing the private keys after the handshake: each side needs its key for every reconnect, and the
  key copy inside the TLS library cannot be reached from safe code.
- Offering two protocol versions in the ALPN list: against the decision to keep the code small.
- Power-state APIs for sleep and wake: they need a run loop or D-Bus for no gain, since waking
  always brings the interfaces back and fires the network watch.
- Extra OSCs sent by `ssj` itself (desktop notification, progress indicator, badge): kept out for now.
- A server-side terminal emulator or `smcup` tracking: against the dumb-pipe rule. Full-screen
  programs are covered by the escape rule above.
- Hole punching for hosts behind NAT: the TCP tier through `ssh` covers them.
- A manifest of server digests inside the client: built and removed again as too complex for what
  the release checksums and attestation already give.
