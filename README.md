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
ssj attach user@host [ID]           # join a running session, for example after a restart
ssj list user@host                  # sessions with state and age
ssj kill user@host [ID]             # end a session
ssj --name work user@host           # join the session `work`, or start it
ssj attach user@host work           # an id or a name
ssj user@host:2222                  # port of the ssh server; also ssh://user@host:2222
ssj --server-path ~/bin/ssj-server user@host
ssj --ssh-command 'ssh -J jump' user@host
ssj --udp-host 203.0.113.7 user@host  # UDP address when it differs from the host name
ssj --install user@host              # install the server first if it is missing
ssj --port-range 62000:62100 user@host
ssj --transport tcp user@host        # auto (default), udp, tcp (ssh -W), or stdio (the ssh channel)
ssj install user@host               # upload the server file to the host
ssj user@host -- make -j8           # run a command in a session that survives jumps
ssj user@host uptime                # the same, when the command has no options of its own
ssj edit                            # edit client.toml in $EDITOR
ssj edit user@host                  # edit server.toml on the host in $EDITOR
ssj install --download user@host    # fetch it from the release over HTTPS first
ssj list --json user@host           # the same list as JSON
```

### Ending and leaving a session

Ending a session stops the shell. Leaving it keeps the shell running.

| Action | Effect |
| --------------------------------------------------- | --------------------------------------------------- |
| `exit`, Ctrl-D on an empty line | Ends the session. `ssj` exits with the shell code. |
| `ssj kill user@host [ID]` | Ends the session from another terminal. |
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

- Ending the shell (`exit`, Ctrl+D), closing the terminal window, and `kill ssj` (SIGHUP or
  SIGTERM) all end the session and its shell, as they would with `ssh`. Only `~.` leaves it
  running on purpose.
- After a lost connection (a network change, a sleeping laptop, a client killed with SIGKILL), the
  server waits for the client and ends the session after the detached timeout (off by default,
  `detached_timeout` in `server.toml`). `ssj list` shows what is left, and `ssj kill` ends it.
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
compress = "off"                      # "off" or "auto": compress while the link is the limit
congestion = "bbr"                    # "bbr" or "cubic": how fast to send (see below)
forward_agent = false                 # let programs on the host use your ssh agent (-A)
```

`$XDG_CONFIG_HOME/ssj/server.toml` on the host:

```toml
port_range = "60000:61000"            # UDP ports the server may use, "0" for any
detached_timeout = "none"             # seconds without a client (default: none)
max_detached = "none"                 # limit even after a deliberate detach
replay_capacity = 4194304             # bytes of output kept for a client that is away
slow_client = "drop"                  # "drop": the job never waits for the link; "wait": like ssh
compress = "off"                      # "off" or "auto": compress output when the link is the limit
congestion = "bbr"                    # "bbr" or "cubic"
live_backlog = 1048576                # bytes that may wait for a connected client (drop)
away_after = 10                       # seconds a silent client may block the output (wait)
shell = "/bin/zsh"                    # default: $SHELL
accept_env = ["TERM", "LANG", "LC_*"] # variables of the client the shell gets
```

### Congestion control

`ssj` sends with BBR by default, which keeps its speed on a link that loses packets for other
reasons than congestion (Wi-Fi, mobile). On a link with 1 % loss it was many times faster than
Cubic. On a clean gigabit link it is about 15 % slower, and on the loopback of one machine much
slower, so `congestion = "cubic"` (or `--congestion cubic`) is there for a wired network. The
client passes its choice to the server it starts, and each side sends with its own.

### Agent forwarding

`ssj -A host` (or `forward_agent = true` in `client.toml`) lets programs on the host, such as
`git` or a second `ssh`, use the ssh agent of your machine. The host never gets the keys, but
anybody who can use the shell's `SSH_AUTH_SOCK` there can have the agent sign while you are
connected, so use it only on hosts you trust. Only listing the keys and signing are passed on;
adding, removing, and locking keys, and keys with destination constraints, do not work.

### Running a command

`ssj user@host -- COMMAND...` runs the command in the session instead of the shell, as
`ssh user@host COMMAND` does, and ends with its exit code. The host's login shell runs it
(`$SHELL -l -c 'COMMAND'`), so pipes, `&&`, and `cd` work. A long `make` or a `tail -f` then
survives a network change or a closed laptop like any other session, and you can join it again
from another terminal (it is a session with a name you find in `ssj list`).

- Options of `ssj` stay where they are (`ssj --local-echo auto host --name x`), so options of the
  command need the `--`: `ssj host -- ls -l`. Without options of its own, `ssj host uptime` works
  too.
- The command has a terminal, as with `ssh -t`: its output has carriage returns, and a program
  can ask for a terminal size. Standard input goes to it as typed.
- `--name` and `--auto-attach` are refused with a command, since a session that runs already would
  not run it.
- A host with the name of one of our commands (`attach`, `list`, `kill`, `install`, `edit`) is
  written before the dashes: `ssj attach -- ls` runs `ls` on the host `attach`, and `ssj attach ls`
  attaches to a session on the host `ls`.

### Editing the config files

`ssj edit` opens `client.toml` in `$EDITOR` (then `$VISUAL`, then `vi`; a command with arguments
such as `code -w` works), and `ssj edit user@host` does the same for `server.toml` on the host. A
new file starts with every setting as a comment. The text is judged as soon as the editor ends:
the client checks `client.toml` itself, and `ssj-server --check-config` on the host checks
`server.toml`, so a typo in a key is shown and never saved. You can then edit again or discard the
change, as with `visudo`. A file is saved by renaming (mode 0600), so a reader never sees half of
it. If the file was changed by somebody else while your editor was open, nothing is overwritten:
the error names a file with your text. Sessions that run keep their settings; new ones use the file. A host that is called `edit` is
reached as `user@edit`.

### Debug output

`ssj -v host` tells the steps, as `ssh -v` does, each with the time since the start:

- `-v`: the bootstrap through `ssh`, the transports in order, the one tried now and why it
  failed, a change of the network that was detected, the socket moving to the new network, a lost
  connection and each attempt to reconnect, forgotten output, and the end of the session.
- `-vv`: also the `ssh` command line (never the pin), the address lookup, the round trip of the
  QUIC handshake, the offsets of the welcome, window size changes, compression, and agent channels.
- `-vvv`: also every frame that is sent or received, by kind and size and never by content.

The log names the session id, and the server process shows the same id in its arguments, so
`ps ax | grep <id>` finds the server process of a session. After a change of the
network the log says how many notices came, whether a socket was moved, and whether the server
answered the probe.

The lines go to the standard error output. A program such as `vim` draws over them, so
`--log-file FILE` writes them to a file (mode 0600) instead. Nothing typed or printed in the session
is logged.

### Local echo

On a slow link a typed character shows only after the server has echoed it. With
`--local-echo auto` (or `local_echo = "auto"` in `client.toml`) `ssj` shows it at once, dimmed, and
the real echo replaces it. It is off by default, because it is a guess.

- It predicts printable ASCII and letters of one column (German umlauts and ß, accented Latin
  letters, Greek, Cyrillic) at the end of a plain shell line. Arrow keys, control keys, backspace,
  and wide characters (Chinese, Japanese, Korean, emoji) stop it until the next Enter. If the
  shell on the host is not set to UTF-8 and echoes the letter differently, the prediction is
  taken back and stops, as for any echo that differs.
- After Enter it shows nothing until the server has echoed one typed character, so a password
  prompt, which does not echo, is never shown. A full-screen program or a cursor movement in the
  output also stops it.
- It takes its drawing back before it shows any output, and after two seconds without an echo.
- A shell that redraws the whole line for every key (some zsh setups) gets no prediction.
- The client has no copy of the screen, so it keeps three columns free at the right edge, where a
  wrap would scroll the screen. A prompt with wide characters can make its estimate of the column
  wrong, and then a prediction near the edge can leave a stray dimmed character until the shell
  redraws the line.

### Compression

On a slow link, `compress = "auto"` (in `client.toml` or `server.toml`, or `--compress auto`)
compresses input and output (zstd) while the link is the limit. A build log or a pasted text
shrinks to a fifth or less. It is off by default: compressing text of an attacker together with a secret can
leak the secret through the size of the data, as with `ssh -C`. Typing is never compressed, and
each frame is compressed alone. See the specification before turning it on for a shell where you
paste secrets or print them next to text from others.

### Jobs while you are away

A job keeps running while no client is connected. The server keeps the newest `replay_capacity`
bytes of its output (4 MiB by default). If the job writes more than that while you are away, the
oldest output is forgotten, and when you come back, the terminal shows
`[ssj: N bytes of output were dropped]` with the exact number. Set a higher `replay_capacity` to
keep more.

### A slow connection

With `ssh`, a slow link slows the job down: the shell waits in `write` until the link has taken
its output, and after Ctrl-C the output that is already queued still has to arrive. `ssj` does not
do that by default (`slow_client = "drop"`). The job always runs at full speed. The server keeps
at most `live_backlog` bytes (1 MiB) of output that the connected client has not taken yet. When
the link is slower than the job, the oldest of the rest is skipped, the terminal shows
`[ssj: N bytes of output were dropped]`, and Ctrl-C stops the output within that backlog.
What the shell wrote before you connected (the replay after `attach` or a reconnect) is not
skipped.

If you need every byte even on a slow link, set `slow_client = "wait"` in `server.toml`, or
`--slow-client wait`: the job then waits for the client, as with `ssh`, and nothing is lost while
you are connected. `away_after` only applies to this setting.

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
