# Roadmap

Everything that is decided but not built yet. Finished work is documented in `SPEC.md` and
`README.md` and is not listed here. An entry is removed when it is implemented and tested.

## Known limits that stay until someone fixes them

- The Linux network watch (netlink) has been compiled but not run.

## Considered and not planned

- Skipping the QUIC retry for new clients: it costs one round trip, and skipping it would let a spoofed address amplify traffic up to three times.
- Larger UDP socket buffers: at 180 MB/s on a clean link the default buffer drops 0.9 percent of the datagrams, and throughput is on par with `ssh`. A request above `net.core.rmem_max` is clamped, and that equals the default (212992) on a stock Linux.
- Zeroing the private keys after the handshake: each side needs its key for every reconnect, and the key copy inside the TLS library cannot be reached from safe code.
- Offering two protocol versions in the ALPN list: against the decision to keep the code small.
- Power-state APIs for sleep and wake: they need a run loop or D-Bus for no gain, since waking always brings the interfaces back and fires the network watch.
- Extra OSCs sent by `ssj` itself (desktop notification, progress indicator, badge): kept out for now.
- `ControlMaster` for the second `ssh` of the TCP and stdio transports: a stale master socket after a network change hangs new `ssh` calls, which is what `ssj` exists to avoid.
- A server-side terminal emulator or `smcup` tracking: against the dumb-pipe rule. Full-screen programs are covered by the escape rule above.
- Hole punching for hosts behind NAT: the TCP tier through `ssh` covers them.
- A manifest of server digests inside the client: built and removed again as too complex for what the release checksums and attestation already give.
