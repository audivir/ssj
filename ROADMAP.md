# Roadmap

Everything that is decided but not built yet. Finished work is documented in `SPEC.md` and
`README.md` and is not listed here. An entry is removed when it is implemented and tested.

Order: performance (2), then the release (3).

## 2. Performance

- BBR instead of Cubic, to be measured under `netem` with loss.
- Shorten the start further (measured in `SPEC.md`, "Start and end of a session"):
  - Start the shell during the bootstrap: the client gives the size, the environment, and the
    agent flag through the `ssh` channel, so that the shell runs its startup files during the
    handshake, and `Hello::Start` only joins it.
  - When UDP does not answer, start the next transport after a short time and keep whichever
    connects first, instead of waiting 1.5 s and then opening a second `ssh`. The second `ssh`
    could use a `ControlMaster` connection of the first.
  - The retry for every new client costs one round trip. Skipping it while few unvalidated
    handshakes are in progress would save that, at the price of an amplification of up to three
    times for a spoofed address, so it needs a decision.
- Larger UDP socket buffers (a safe setter such as `socket2`), to be measured on Linux.
- Run `bench.yml` once on GitHub to confirm the numbers on a clean machine. It has never run, and
  the benchmark by hand in containers showed no difference between glibc and musl, so nothing
  depends on it. Then gate on the numbers.

## 3. Release

- Push the repository to a remote so that `ci.yml`, `fuzz.yml`, and `supply-chain.yml` run. They have never run, and the Linux tests have never been executed anywhere. Expect to fix things.
- Manual checks that no CI machine can do: the macOS and Linux network watch (switch Wi-Fi, sleep the laptop), listed in `cargo run -p ssj-client --example netwatch`.
- A tagged release (`v0.1.0`) and a first run of the release workflow. The `PROTOCOLS.md` row and the golden file for protocol 1 exist.

## Later, optional

- **`ssj install --download`**, fetching the server file from the release of the same version and checking it against `SHA256SUMS`. It needs an HTTPS client in the binary or a call to `curl`, and the archive already carries the server files, so it waits for a request.

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
