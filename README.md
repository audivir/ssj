# ssj

Session survives jumps: a remote shell for flaky networks. The session survives roaming, IP
changes, sleep, and packet loss, and the native scrollback of the local terminal keeps working.
The server runs without root and the transport is QUIC over UDP.

_Status:_ under development, and not released yet. You can already use it: `ssj` bootstraps the
server through `ssh`, connects over QUIC (falling back to TLS over TCP and to the `ssh` channel
when UDP is blocked), passes the terminal through, reconnects after a lost connection or a change
of network without losing or repeating input, names and manages sessions (`list`, `attach`,
`kill`), and installs the server on the host. The code has been run on macOS only; the Linux
builds are compiled and tested by CI, which has not run yet. See `SPEC.md` for the design and the
threat model.

## Prerequisites

- Rust toolchain (edition 2024)
- `ssh` access to the remote host, used for the bootstrap
- A reachable UDP port range on the remote host (default 60000 to 61000, configurable)

## Installation

```shell
cargo install --path crates/ssj-client
cargo install --path crates/ssj-server
```

Put the server file for the host next to `ssj` (a release archive does this), then let
`ssj install user@host` upload it to `~/.local/share/ssj/bin/ssj-server-p1`. No root is needed.
The upload is checked with SHA-256 on the host, tested by running it, and only then moved into
place, so a broken transfer never replaces a working server.
Check the downloaded files against `SHA256SUMS` of the release before you use them.

A release archive `ssj-<version>-<target>.tar.gz` holds `ssj`, the server files of all builds, the
man page `ssj.1`, and `completions/` for Bash, Zsh, and Fish. Put `ssj.1` into a `man1` directory
and load the completion of your shell (`source completions/ssj.bash`, a copy of `_ssj` in `fpath`,
or `completions/ssj.fish` in `~/.config/fish/completions`). Session ids and names are completed
from a list that `ssj list` keeps in `$XDG_CACHE_HOME/ssj/sessions/`; it is refreshed in the
background without prompts, and a session that was never listed on a host is not offered.

When the server is missing, `ssj user@host` offers to install it, as set by `auto_install` in
`$XDG_CONFIG_HOME/ssj/client.toml`:

```toml
auto_install = "ask"   # "ask" (default), "always", or "never"
```

- `ask`: on a terminal you are asked, and only `y` or `yes` installs. Without a terminal nothing
  is installed, and the message tells you what to do.
- `always`: installs without asking. `never`: never installs.
- `--install` and `--no-install` override the setting for one run.
- `ssj attach`, `ssj list`, and `ssj kill` never install anything.

## Usage

```shell
ssj user@host                       # start a session
ssj attach [ID] user@host           # join a running session, for example after a restart
ssj list user@host                  # sessions with state and age
ssj kill [ID] user@host             # end a session
ssj --name work user@host           # join the session `work`, or start it
ssj attach work user@host           # an id or a name
ssj user@host:2222                  # port of the ssh server; also ssh://user@host:2222
ssj --server-path ~/bin/ssj-server user@host
ssj --ssh-command 'ssh -J jump' user@host
ssj --udp-host 203.0.113.7 user@host  # UDP address when it differs from the host name
ssj --install user@host              # install the server first if it is missing
ssj --port-range 62000:62100 user@host
ssj --transport tcp user@host        # auto (default), udp, tcp (ssh -W), or stdio (the ssh channel)
ssj install user@host               # upload the server file to the host
ssj list --json user@host           # the same list as JSON
```

### Ending and leaving a session

Ending a session stops the shell. Leaving it keeps the shell running.

| Action | Effect |
| --------------------------------------------------- | --------------------------------------------------- |
| `exit`, Ctrl-D on an empty line | Ends the session. `ssj` exits with the shell code. |
| `ssj kill [ID] user@host` | Ends the session from another terminal. |
| `~.` at the start of a line | Detaches on purpose. The shell keeps running. |
| Closing the terminal, `SIGHUP`, `SIGTERM`, `SIGINT` | Disconnects. The shell keeps running. |

The `~` is the escape character, as in `ssh`. It counts only at the start of a line:

| Keys | Effect |
| ---- | --------------------------------------- |
| `~.` | Detach and exit with code 0. |
| `~~` | Send a literal `~`. |
| `~?` | Print this list. |

Any other character after `~` is sent as typed. Set another escape character with
`--escape-char C`, or turn the feature off with `--escape-char none`. The `escape_char` setting in
`client.toml` does the same for every run.

### Connection problems

- When the server stops answering, the window title of the terminal shows `no response from the server` after a short time that depends on the speed of your link. When the connection is lost,
  it shows `connection lost, reconnecting`. The session itself keeps running, and what you type
  meanwhile is sent when the connection is back. `~.` works even without a connection.
- `ssj` notices when the network changes or the machine wakes from sleep (through the network
  state of macOS or netlink on Linux, and a clock check everywhere) and moves to the new path at
  once instead of waiting for a timeout.
- The message is written to the window title, so it never draws over the program that runs in the
  terminal. If the output stops in the middle of an escape sequence, the message waits. When the
  sequence is still open after two seconds, `ssj` cancels it on the terminal and drops the rest of
  it when it arrives, so a stalled link shows its message and no half sequence appears as text.

### Timeouts and detached sessions

- After an involuntary disconnect, the server waits for the client and ends the session after the
  detached timeout (off by default, `detached_timeout` in `server.toml`).
- After `~.` there is no timeout. The shell runs until you attach again, or until you end it with
  `ssj kill`. The server closes its UDP port on detach, so a detached session listens on nothing.
  Attaching again goes through `ssh` and opens a new port.
- `ssj list user@host` shows every session with its state and age. An administrator or you can
  set `max_detached` in `server.toml` as an upper limit that also applies to deliberate detaches.

### Blocked UDP and jump hosts

`ssj` tries UDP first. When UDP is blocked, or `ssh` goes through a jump host, it falls back to TLS
over TCP through `ssh -W`, and then to the `ssh` channel itself. These two start a new `ssh` for
every connection and cannot ask for a passphrase, so use an agent or a shared connection
(`ControlMaster auto`, `ControlPersist 10m` in your `ssh` configuration). Choose a transport with
`--transport` or `transport` in `client.toml`. See the specification for the details.

### Security notes

- Authentication is done by `ssh`. `ssj` stores no passwords or keys. Each session uses fresh
  certificates on both sides, which are pinned through the `ssh` bootstrap, and nothing is
  written to disk.
- Only the holder of the pinned client key can open a session. An open UDP port allows handshake
  attempts only, and these are limited by address validation and strict resource limits. The TCP
  port listens on the loopback address only and needs the same certificate.
- A session without timeout is a running shell in the background. Use `ssj list` to find forgotten
  sessions.
- Terminal escape sequences in the output of the remote host (such as clipboard writes) reach
  your terminal, as with `ssh`.

### Config files

Both files are flat `key = value` lines, a strict subset of TOML. A typo in a key is an error with
the line number, and a file that others can write is refused. Flags on the command line override
the file.

`$XDG_CONFIG_HOME/ssj/client.toml`:

```toml
auto_install = "ask"                  # "ask", "always", or "never"
escape_char = "~"                     # one character, or "none"
ssh_command = "ssh -J jump"           # replaces ssh
send_env = ["TERM", "LANG", "LC_*"]   # variables sent to the server
transport = "auto"                    # "auto", "udp", "tcp", or "stdio"
auto_attach = false                   # `ssj host` joins the only session without a client
local_echo = "off"                    # "off" or "auto": show typed characters at once
```

`$XDG_CONFIG_HOME/ssj/server.toml` on the host:

```toml
port_range = "60000:61000"            # UDP ports the server may use, "0" for any
detached_timeout = "none"             # seconds without a client (default: none)
max_detached = "none"                 # limit even after a deliberate detach
replay_capacity = 4194304             # bytes of output kept for a client that is away
away_after = 10                       # seconds a silent client may block the output
shell = "/bin/zsh"                    # default: $SHELL
accept_env = ["TERM", "LANG", "LC_*"] # variables of the client the shell gets
```

### Local echo

On a slow link a typed character shows only after the server has echoed it. With
`--local-echo auto` (or `local_echo = "auto"` in `client.toml`) `ssj` shows it at once, dimmed, and
the real echo replaces it. It is off by default, because it is a guess.

- It predicts only printable ASCII at the end of a plain shell line. Arrow keys, control keys,
  backspace, and non-ASCII characters stop it until the next Enter.
- After Enter it shows nothing until the server has echoed one typed character, so a password
  prompt, which does not echo, is never shown. A full-screen program or a cursor movement in the
  output also stops it.
- It takes its drawing back before it shows any output, and after two seconds without an echo.
- A shell that redraws the whole line for every key (some zsh setups) gets no prediction.
- The client has no copy of the screen, so it keeps three columns free at the right edge, where a
  wrap would scroll the screen. A prompt with wide characters can make its estimate of the column
  wrong, and then a prediction near the edge can leave a stray dimmed character until the shell
  redraws the line.

### Jobs while you are away

A job keeps running while no client is connected. The server keeps the newest `replay_capacity`
bytes of its output (4 MiB by default). If the job writes more than that while you are away, the
oldest output is forgotten, and when you come back, the terminal shows
`[ssj: N bytes of output were dropped]` with the exact number. While you are connected, nothing is
dropped: a slow connection slows the job down, as with `ssh`. Set a higher `replay_capacity` to
keep more.

`accept_env` can only be set in the file, never on the command line, because the command line of
the server is built by the client.

### Benchmark

`bench/bench.py --host user@host` compares `ssh` with `ssj` over UDP, TCP, and the `ssh` channel:
bulk output, keystroke latency, and an 8 MiB paste. It needs Python 3 and the release builds in
`target/release`. See the specification for the options and the first numbers.

### Tests

The tests start real programs and wait for real clocks, so they run with
[`cargo nextest`](https://nexte.st) (`cargo nextest run --workspace`, about 25 seconds on a laptop).
It runs each test in its own process and stops and names a test that hangs. The settings are in
`.config/nextest.toml`; CI uses the `ci` profile, which retries a failed test once and lists it as
flaky. `cargo test` works too, but it does not stop a hanging test.

### Files

Paths follow the XDG Base Directory specification: `$XDG_CONFIG_HOME/ssj/` (`client.toml`,
`server.toml`), `$XDG_STATE_HOME/ssj/`, and `$XDG_RUNTIME_DIR/ssj/` (without it
`$XDG_STATE_HOME/ssj/run/<host>/`, or `/tmp/ssj-<uid>/` if the path is too long for a socket).

## License

MIT
