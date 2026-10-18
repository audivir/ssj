# Roadmap

Everything that is decided but not built yet. Finished work is documented in `SPEC.md` and
`README.md` and is not listed here. An entry is removed when it is implemented and tested.

Order: performance (2), then the release (3).

## 2. Performance

- Do not let a slow connection slow the job (see the design below). The server keeps a bounded
  backlog of unsent output and drops the oldest of it, with a new `Gap` frame so that the client
  shows the notice. This replaces the backpressure while connected and `away_after`.
- Compression of the output direction (`flate2` with the Rust backend), only when there is unsent
  backlog, negotiated in the hello. Add an `ssh -C` row to the benchmark with a text producer.
- BBR instead of Cubic, to be measured under `netem` with loss.
- Measure and shorten the start (bootstrap through `ssh`, then the handshake).
- Larger UDP socket buffers (a safe setter such as `socket2`), to be measured on Linux.
- Run `bench.yml` once on GitHub, compare the glibc and musl servers, choose the default build for
  automatic installs, and then gate on the numbers.

## 3. Release

- Push the repository to a remote so that `ci.yml`, `fuzz.yml`, and `supply-chain.yml` run. They have never run, and the Linux tests have never been executed anywhere. Expect to fix things.
- Manual checks that no CI machine can do: the macOS and Linux network watch (switch Wi-Fi, sleep the laptop), listed in `cargo run -p ssj-client --example netwatch`.
- A tagged release (`v0.1.0`) and a first run of the release workflow. The `PROTOCOLS.md` row and the golden file for protocol 1 exist.

## Later, optional

- **`ssj install --download`**, fetching the server file from the release of the same version and checking it against `SHA256SUMS`. It needs an HTTPS client in the binary or a call to `curl`, and the archive already carries the server files, so it waits for a request.
- **SSH agent forwarding** over a dedicated stream. Large and security sensitive.

## Known limits that stay until someone fixes them

- The Linux network watch (netlink) has been compiled but not run.

## Considered and not planned

- Zeroing the private keys after the handshake: each side needs its key for every reconnect, and the key copy inside the TLS library cannot be reached from safe code.
- Offering two protocol versions in the ALPN list: against the decision to keep the code small.
- Power-state APIs for sleep and wake: they need a run loop or D-Bus for no gain, since waking always brings the interfaces back and fires the network watch.
- Extra OSCs sent by `ssj` itself (desktop notification, progress indicator, badge): kept out for now.
- A server-side terminal emulator or `smcup` tracking: against the dumb-pipe rule. Full-screen programs are covered by the escape rule above.
- Hole punching for hosts behind NAT: the TCP tier through `ssh` covers them.
- A manifest of server digests inside the client: built and removed again as too complex for what the release checksums and attestation already give.
