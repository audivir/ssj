# Roadmap

Everything that is decided but not built yet. Finished work is documented in `SPEC.md` and
`README.md` and is not listed here. An entry is removed when it is implemented and tested.

Order: performance (2), then the release (3).

## 2. Performance

- Run `bench.yml` once on GitHub (it has never run). Compare the glibc and musl builds of the
  server, with and without `netem`, and choose the default build for automatic installs from the
  numbers. `SPEC.md` still says musl.
- Find why bulk output and the paste are about 15 percent slower than `ssh` on loopback (a profile
  of the client and the server under the bulk case is the first step).
- Then gate on the numbers (the proposal suggested 5 percent for throughput and 1 ms for latency,
  which only make sense after the first point).

## 3. Release

- Push the repository to a remote so that `ci.yml`, `fuzz.yml`, and `supply-chain.yml` run. They
  have never run, and the Linux tests have never been executed anywhere. Expect to fix things.
- Manual checks that no CI machine can do: the macOS and Linux network watch (switch Wi-Fi, sleep
  the laptop), listed in `cargo run -p ssj-client --example netwatch`.
- A tagged release (`v0.1.0`) and a first run of the release workflow. The `PROTOCOLS.md` row and
  the golden file for protocol 1 exist.

## Later, optional

- **Speculative local echo**, off by default (`local_echo = "off" | "auto"`). Predict only printable
  characters on a plain shell line, drawn dim, and confirm against the server output. Epoch rule of
  mosh: after Enter, show nothing until the server has echoed one character, which closes the race
  with password prompts. The server reports `ECHO` and canonical mode from `tcgetattr` as an extra
  signal. Prediction stops at the first escape sequence in the output, which covers full-screen
  programs. Needs a new server frame, so it lands before the first release or waits for protocol 2.
- **`ssj install --download`**, fetching the server file from the release of the same version and
  checking it against `SHA256SUMS`. It needs an HTTPS client in the binary or a call to `curl`, and
  the archive already carries the server files, so it waits for a request.
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
