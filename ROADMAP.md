# Roadmap

Everything that is decided but not built yet. Finished work is documented in `SPEC.md` and
`README.md` and is not listed here. An entry is removed when it is implemented and tested.

Order: performance (2), then the release (3).

## 2. Performance

- The retry for every new client costs one round trip. Skipping it while few unvalidated
  handshakes are in progress would save that, at the price of an amplification of up to three
  times for a spoofed address, so it needs a decision (recommended: keep the retry).
- Larger UDP socket buffers (a safe setter such as `socket2`), to be measured on Linux.

## 3. Release

- Manual checks that no CI machine can do: the macOS and Linux network watch (switch Wi-Fi, sleep the laptop), listed in `cargo run -p ssj-client --example netwatch`; a real jump host.
- A tagged release (`v0.1.0`). The `PROTOCOLS.md` row and the golden file for protocol 1 exist.

## Known limits that stay until someone fixes them

- The Linux network watch (netlink) has been compiled but not run.

## Considered and not planned

- Zeroing the private keys after the handshake: each side needs its key for every reconnect, and the key copy inside the TLS library cannot be reached from safe code.
- Offering two protocol versions in the ALPN list: against the decision to keep the code small.
- Power-state APIs for sleep and wake: they need a run loop or D-Bus for no gain, since waking always brings the interfaces back and fires the network watch.
- Extra OSCs sent by `ssj` itself (desktop notification, progress indicator, badge): kept out for now.
- `ControlMaster` for the second `ssh` of the TCP and stdio transports: a stale master socket after a network change hangs new `ssh` calls, which is what `ssj` exists to avoid.
- A server-side terminal emulator or `smcup` tracking: against the dumb-pipe rule. Full-screen programs are covered by the escape rule above.
- Hole punching for hosts behind NAT: the TCP tier through `ssh` covers them.
- A manifest of server digests inside the client: built and removed again as too complex for what the release checksums and attestation already give.
