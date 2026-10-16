# ssj

Session survives jumps: a remote shell for flaky networks. The session survives roaming, IP
changes, sleep, and packet loss, and the native scrollback of the local terminal keeps working.
The server runs without root and the transport is QUIC over UDP.

_Status:_ under development. The protocol, the session core of the server, and the checks are
implemented and tested. The `ssj` and `ssj-server` programs themselves do not work yet, so the
usage below describes the planned behavior. See `SPEC.md` for the design and the threat model.

## Prerequisites

- Rust toolchain (edition 2024)
- `ssh` access to the remote host, used for the bootstrap
- A reachable UDP port range on the remote host (default 60000 to 61000, configurable)

## Installation

```shell
cargo install --path crates/ssj-client
cargo install --path crates/ssj-server
```

Install the server on the remote host as the unprivileged user, or let `ssj install user@host`
upload it to `~/.local/share/ssj/bin/`. No root is needed.

## Usage

```shell
ssj user@host                       # start or join a session
ssj --port-range 62000:62100 user@host
ssj list user@host                  # sessions with state and age
ssj kill [ID] user@host             # end a session
ssj install user@host               # upload the server
```

### Ending and leaving a session

Ending a session stops the shell. Leaving it keeps the shell running.

| Action                                              | Effect                                              |
| --------------------------------------------------- | --------------------------------------------------- |
| `exit`, Ctrl-D on an empty line                     | Ends the session. `ssj` exits with the shell code.  |
| `ssj kill [ID] user@host`                           | Ends the session from another terminal.             |
| `~.` at the start of a line                         | Detaches on purpose. The shell keeps running.       |
| Closing the terminal, `SIGHUP`, `SIGTERM`, `SIGINT` | Disconnects. The shell keeps running.               |

The `~` is the escape character, as in `ssh`. It counts only at the start of a line:

| Keys | Effect                                  |
| ---- | --------------------------------------- |
| `~.` | Detach and exit with code 0.            |
| `~~` | Send a literal `~`.                     |
| `~?` | Print this list.                        |

Any other character after `~` is sent as typed. Set another escape character with
`--escape-char C` or `escape_char` in `client.toml`, or turn the feature off with `none`.

### Timeouts and detached sessions

- After an involuntary disconnect, the server waits for the client and ends the session after the
  detached timeout (configurable, `detached_timeout` in `server.toml`).
- After `~.` there is no timeout. The shell runs until you attach again, or until you end it with
  `ssj kill`. The server closes its UDP port on detach, so a detached session listens on nothing.
  Attaching again goes through `ssh` and opens a new port.
- `ssj list user@host` shows every session with its state and age. An administrator or you can
  set `max_detached` in `server.toml` as an upper limit that also applies to deliberate detaches.

### Security notes

- Authentication is done by `ssh`. `ssj` stores no passwords or keys. Each session uses fresh
  certificates on both sides, which are pinned through the `ssh` bootstrap, and nothing is
  written to disk.
- Only the holder of the pinned client key can open a session. An open UDP port allows handshake
  attempts only, and these are limited by address validation and strict resource limits.
- A session without timeout is a running shell in the background. Use `ssj list` to find forgotten
  sessions.
- Terminal escape sequences in the output of the remote host (such as clipboard writes) reach
  your terminal, as with `ssh`.

### Files

Paths follow the XDG Base Directory specification: `$XDG_CONFIG_HOME/ssj/` (`client.toml`,
`server.toml`), `$XDG_STATE_HOME/ssj/`, and `$XDG_RUNTIME_DIR/ssj/`.

## License

MIT
