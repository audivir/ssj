# ssj

Session survives jumps: a remote shell for flaky networks. The session survives roaming, IP changes,
sleep, and packet loss, and the native scrollback of the local terminal keeps working.
The server runs without root and the transport is QUIC over UDP.

See `SPEC.md` for the design and the threat model.

## Prerequisites

- Rust toolchain (edition 2024)
- `ssh` access to the remote host, used for the bootstrap
- A reachable UDP port range on the remote host (default 60000 to 61000, configurable)

## Installation

```shell
cargo install --path crates/ssj-client
cargo install --path crates/ssj-server
```

Install `ssj-server` on the remote host as the unprivileged user.

## Usage

Not implemented yet. The planned interface is `ssj user@host`, with `--port-range LOW:HIGH` to
select the UDP ports.

## License

MIT
