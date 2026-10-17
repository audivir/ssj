# Implementation plan

This plan covers what is left of `SPEC.md`, the findings of `PROBLEMS.md`, and the features of
`FALLBACK_SPEC_PROPOSAL.md`. I checked each claim of those two documents against the code. Some
are right, some are partly right, and a few are wrong or conflict with decisions you made earlier.
The verdicts come first, then the decisions I need from you, then the phases.

## 1. Verdicts on the findings

The code references below were checked in this repository.

| ID | Verdict | Why | Action |
| --- | --- | --- | --- |
| ARCH-1 | **Right, serious** | A full replay buffer stops reading the PTY, so a detached job freezes after 1 MiB of output. Dropping output instead makes `ReplayGap` fatal in the client. | Phase 1 |
| ARCH-2 | **Right** | The kernel PTY cannot report `smcup`. A late `ECHO` change makes a speculative echo print a password. | Do not build speculative echo (decision D3) |
| ARCH-3 | **Right** | Already listed as a known limit in `SPEC.md`. The title stack is not supported everywhere. | Phase 2 |
| NET-1 | **Right for the UDP range, wrong for the proposed fix** | The range is small and squattable. But a firewall rule needs a known range, so `bind(0)` cannot replace it on UDP. | Phase 3 (details in D4) |
| NET-2 | **Right, for the TCP tier only** | QUIC has its own liveness. A TCP socket does not. | Phase 4 |
| NET-3 | **Partly right** | A `Banner` goes to stderr before login, and `/etc/motd` is not printed for a command without a terminal. Output of `.bashrc` is the real hazard. | Phase 4, the sync header is still needed |
| NET-4 | **Right** | `resolve` runs once, at `run.rs:280`. | Phase 2 |
| NET-5 | **Right** | A server behind NAT cannot take inbound UDP. | Solved by the TCP tier (Phase 4) |
| PROTO-1 | **Right, but conflicts with your decision** | One protocol per binary was your choice to keep the code slim. | Decision D1 |
| PROTO-2 | **Mostly irrelevant** | Our `ssh` runs with `-T`, so it has no terminal and does not process `~.`. | Add `-e none` anyway, it costs one argument (Phase 2) |
| PROTO-3 | **Right** | `SIGWINCH` is forwarded unthrottled (`run.rs:739`). | Phase 2 |
| SEC-1 | **Partly right** | Unix sockets can exist on NFS. The real problems are the path length limit (104 bytes on macOS, 108 on Linux) and a home directory shared by several hosts. The claimed `EOPNOTSUPP` is doubtful. | Phase 3 |
| SEC-2 | **Plausible, cheap to fix** | I could not verify the CVEs named. A strict check of the values of `TERM`, `LANG`, and `LC_*` is cheap and harmless. | Phase 3 |
| UX-1 | **Right, only if completions exist** | | Phase 6 |

Ideas of the fallback proposal:

| Idea | Verdict |
| --- | --- |
| Tier 1, TCP over `ssh -W` | **Build.** It is the only way through a jump host, since OpenSSH cannot forward UDP. |
| Tier 2, stdio stream | **Build, after Tier 1.** About 200 lines, and it works wherever `ssh` works. |
| Speculative local echo | **Do not build.** See D3. |
| OSC integration (9.5) | **Nothing to build.** Pass-through already works, which is the point of 9.6. |
| Wake-from-sleep trigger through OS power APIs (9.3) | **Not as written.** It needs FFI and `unsafe`. A clock-jump check does the same without it. |
| `ssh -G` lookup (9.1) | **Build.** It also decides the transport. |
| Auto-attach (9.2) | **Build.** Small. |
| Agent forwarding (9.4) | **Defer.** Large and security sensitive. Not in this plan. |
| `clap`, destination parser, completions, man page (9.7) | **Build, with limits.** See D5. |
| Session names (9.8) | **Build.** It needs no wire change. |
| `.tar.gz` releases (9.7) | **Build.** The archive must hold the server builds for all host systems. |

## 2. Decisions

| ID | Decision |
| --- | --- |
| D1 | One protocol per binary, as before. The handshake error names the release to use, and old server files stay side by side. Revisit at the first protocol bump. |
| D2 | **No default detached timeout.** `detached_timeout` defaults to `none`, and `max_detached` stays an optional hard limit. **Root is allowed but warned about** on stderr and in the log, and `--allow-root` is no longer needed. |
| D3 | Speculative echo is not part of this plan's phases. It is an optional late feature (Phase 9), opt-in, with the epoch rule of mosh: no prediction is shown after Enter until the server has echoed one character. |
| D4 | The UDP port range stays configurable with the default 60000 to 61000. `port_range = "0"` (or `--port-range 0`) means any port the kernel gives. There is no `--any-port` flag. |
| D5 | The client CLI moves to `clap`, with completions and a man page generated in `build.rs`. |
| D6 | Wake and network change are handled through platform APIs where available (macOS, Linux), behind one platform-independent trigger, with the clock-jump check as an always-on fallback. |

## 3. Phases

Each phase ends with passing tests, the pre-commit hooks (`prek run --all-files`, which found
real problems last time), `cargo deny`, and a commit of only the files the phase touched.

### Phase 0. Housekeeping (small, do first)

- Push the repository to a remote so that `ci.yml`, `fuzz.yml`, and `supply-chain.yml` run at
  all. They have never run, and the Linux tests have never been executed anywhere.
- Fix what the first CI run reports. Expect work here: the musl test jobs and the `ubuntu-24.04-arm`
  runners are unproven.
- Decide what happens to `PROBLEMS.md` and `FALLBACK_SPEC_PROPOSAL.md`: they are currently
  rewritten by the Markdown hook. Exclude them from the hook, or accept the formatting.
- Settle D1, D2, and D3, and record them in `SPEC.md`.

### Phase 1. Do not freeze a detached job (ARCH-1)

This is the most important bug. A background build must keep running while no client is connected.

- Replace the rule "never drop output" by two modes in `session.rs`. While a client is connected,
  keep backpressure. While none is connected, drop the oldest output when the buffer is full and
  keep reading the PTY.
- On `Resume` with an offset before `output.start()`, accept it, answer with `Welcome` at
  `output.start()`, and send an inline notice `[ssj: N bytes of output were dropped]` before the
  data. The client takes the new offset instead of failing.
- `check_welcome` must allow the moved output offset for `Resume`. This changes the meaning of
  `Welcome`, so regenerate the golden file (the protocol is unreleased, so `PROTOCOL` stays 1).
- Make the capacity configurable (`replay_capacity` in `server.toml`, default 4 MiB).
- Tests: a job that writes 10 MiB while the client is away finishes; the client reconnects and
  sees the notice with the right count; no frame is duplicated; a connected client still gets
  backpressure. Mutation check: restoring the old rule makes the first test hang.
- Update `SPEC.md` sections "Session core" and "Hardening requirements", which promise the old rule.

### Phase 2. Client robustness (NET-4, PROTO-2, PROTO-3, ARCH-3)

- **Re-resolve on reconnect.** Resolve the host inside the reconnect loop, on every third attempt.
  Test with a resolver stub that changes its answer.
- **Debounce resizes.** A trailing-edge timer of 50 ms in the client. The server also coalesces
  resizes that arrive while it is busy. Test: 100 resizes in 10 ms produce at most two frames.
- **`-e none`** in every `ssh` invocation of the client, with a test on the argument list.
- **Safe status display.** Write the status only when the output stream is at a frame boundary
  and the last bytes were not inside an escape sequence. This needs a minimal state tracker for
  "inside an escape or OSC sequence" (about 60 lines, no screen state). Replace the title stack by
  setting the title and restoring the remembered title with OSC 0 (decision of `PROBLEMS.md` 6.3,
  which I agree with). Test with a stream cut inside an OSC 52 sequence.
- **Wake and network change (D6).** One `Trigger` channel in the client ("network changed" or
  "woke up"). On a trigger the client rebinds its UDP socket with `Endpoint::rebind`, so that QUIC
  probes the new path at once, and sends a ping, with a short reconnect probe if that fails. Sources:
  - the clock-jump check (wall clock against monotonic clock) in the ping timer, always on;
  - macOS: network change notifications through the `system-configuration` crate, behind a cargo
    feature;
  - Linux: route and address changes through netlink, behind a cargo feature.
    Our crates stay free of `unsafe`; the dependencies hold it, so each one goes through `cargo deny`
    and a review. A power-state API is not used: waking always brings interfaces back, which fires the
    network trigger. The core and the clock-jump source get automated tests with an injected source.
    The platform adapters can only be tested by hand (sleep the laptop, switch Wi-Fi), so the plan
    lists that as a manual check in the release checklist.

### Phase 3. Server hardening (SEC-1, SEC-2, NET-1, D2)

- Control socket path: if the path would exceed the limit of `sun_path`, use `/tmp/ssj-<uid>/`
  with mode 0700 and the existing ownership and mode checks. Test with a long `XDG_RUNTIME_DIR`.
- Detect a state directory shared between hosts by writing the host name into the socket
  directory and refusing a directory that belongs to another host.
- Check the values of `TERM`, `LANG`, and `LC_*` before they reach the shell: allowed characters
  `A-Za-z0-9._@-`, no path separators, length limit. Reject the whole set, as for the other limits.
- Root is allowed (D2): print a warning to stderr and put it in the first line of the log, and
  remove `--allow-root`. Change the default `detached_timeout` to `none`.
- `port_range = "0"` for any port (D4), and a clear message when the range is exhausted that
  suggests setting it to 0 or widening it.
- Key zeroization of the private keys after the handshake, using `zeroize` if the dependency
  review passes, otherwise by dropping them early. Be honest in the docs about what is not covered.

### Phase 4. Transport tiers (NET-2, NET-3, NET-5, jump hosts)

This is the largest phase. It stays small if the session core is untouched: it already works on
any pair of byte streams.

1. **A stream abstraction in `ssj-net`.** One `Connection` type from `quinn` streams or from a
   `tokio_rustls` stream. The session core and the client loop use only that type.
1. **Bootstrap line v2.** `SSJ CONNECT <udp|none> tcp:<port> <fingerprint>`, with a new strict
   parser and fuzz target. The server binds a TCP listener on `127.0.0.1:0` (kernel assigned) next
   to the UDP socket. `--no-udp` and `--no-tcp` for hosts that want only one.
1. **Tier 1, TCP through `ssh -W`.** The client opens `ssh -e none -W localhost:<port>` and
   runs mutual TLS 1.3 over it with the same pinned verifier. Set `TCP_NODELAY`. Add the
   application deadline from Phase 2 so that a half-open socket is noticed within 5 seconds.
1. **Tier 2, stdio.** `ssj-server --stdio` with the sync header from the proposal, and a client
   reader that skips everything before it. A session over stdio needs the server process to
   outlive the `ssh` channel, so the stdio mode attaches to the running daemon through the control
   socket instead of starting a new one.
1. **Selection.** `--transport auto|udp|tcp|stdio`. `auto` uses `ssh -G` (Phase 5) to see a
   jump host and goes straight to Tier 1. Otherwise it tries UDP for 1.5 seconds, then Tier 1,
   then Tier 2 when forwarding is refused.
1. **Tests** with a fake `ssh` that implements `-W` with a local relay: jump host, blocked UDP
   (a proxy that drops datagrams), forbidden forwarding, and a banner printed by the shell
   profile before the sync header. Reconnect works on every tier, including a network change.
1. **Mutation checks** on the sync-header skip and on the transport fallback order.

### Phase 5. Naming, destinations, and `ssh -G` (9.1, 9.2, 9.8)

- `ssh -G` lookup for `hostname`, `user`, `port`, `proxyjump`, `proxycommand`. This also fixes the
  UDP address for aliases and replaces `--udp-host` as the default.
- `--name` for `ssj`, stored by the server (`--name` argument, no wire change), shown by `ssj list`,
  accepted by `attach` and `kill`. Names are unique per user and checked like session ids.
- `ssj <host>` with `--name` attaches when the session exists and creates it otherwise.
- `auto_attach = true` in `client.toml`.
- Destination parser for `user@host:port`, `ssh://`, and IPv6 literals, with the option-injection
  guard kept. Fuzz it.
- Control protocol: `SessionInfo` gains the name. This is local to one server file, so it does
  not touch `PROTOCOL`.

### Phase 6. CLI and packaging (9.7, 9.9, D5)

- If D5 is yes: move to `clap`, keep every existing flag and test, generate Bash, Zsh, and Fish
  completions and the man page in `build.rs`.
- `ssj list --json`, and the completion cache with the single-flight lock and `BatchMode` from
  `PROBLEMS.md` UX-1.
- Release workflow: one `.tar.gz` per client platform with the client, the **server builds for
  all host systems** (the installer needs them next to the client), man page, completions,
  `README.md`, `LICENSE`, and `SHA256SUMS`. Keep the attestation step.
- Optional, low priority: `ssj install --download` fetching the server file from the release of
  the same version, verified against `SHA256SUMS`. Skipped unless you want it.

### Phase 7. Performance gates (spec section 7 of the proposal)

- Benchmark harness as an ignored test plus a CI job, comparing `ssh`, `ssj` on UDP, and `ssj` on
  TCP for bulk output, keystroke latency, and the 8 MiB paste under `tc netem` (Linux only).
- Run the same on glibc and musl builds to settle the open question from the spec, and choose the
  default for automatic installs from the numbers.
- Gates: fail the job when throughput is more than 5 percent below `ssh` on loopback, or latency
  is more than 1 ms above it. I would first run it without gates to see real numbers, since
  these thresholds come from the proposal and nobody measured them.

### Phase 8. Documentation sync and release

- Bring `SPEC.md` and `README.md` in line with what exists, and remove statements that became
  false (the replay rule, the title stack, the single port range).
- Record the final decisions D1 to D5.
- `PROTOCOLS.md` row, golden file, a tagged release, and a first run of the whole release workflow.

### Phase 9. Optional: speculative local echo (D3)

Only after the release, and only if latency over bad links still hurts.

- Off by default (`local_echo = "off" | "auto"` in `client.toml`).
- Predict only for printable characters on a plain shell line, drawn dim; confirm against the
  server output, and wipe on anything unexpected.
- Epoch rule: after Enter, show nothing until the server has echoed one character in the new
  epoch. This closes the password race without any knowledge of `smcup`.
- The server reports `ECHO` and canonical mode from `tcgetattr` as an extra signal; it is an
  addition to the epoch rule, not a replacement. Full-screen programs are covered by the rule that
  prediction stops at the first escape sequence in the output.
- Needs a protocol change (a new server frame), so it must land before the first release or wait
  for protocol 2. Decide this when Phase 8 is done.

## 4. Order and size

| Phase | Depends on | Size |
| --- | --- | --- |
| 0 Housekeeping | | small |
| 1 Detached jobs | | medium, important |
| 2 Client robustness | | medium |
| 3 Server hardening | | small |
| 4 Transport tiers | 2 | large |
| 5 Names and `ssh -G` | | medium |
| 6 CLI and packaging | 5 | medium |
| 7 Performance | 4 | medium |
| 8 Docs and release | all | small |
| 9 Speculative echo (optional) | 8 | medium |

Phases 1, 2, 3, and 5 are independent and can run in any order. I would do 0, 1, 2, 3 first,
because they fix bugs in what already exists, then 4 and 5 for the features, then 6, 7, and 8.

## 5. Risks

- **Phase 4 can grow.** The two extra tiers touch the client loop, the server launch code, and
  the bootstrap. The stream abstraction is what keeps it contained. If it spreads into the
  session core, stop and redesign.
- **Phase 2 status tracking** adds a small escape-sequence tracker to a client that is otherwise
  a dumb pipe (9.6 of the proposal). It tracks state only to decide when to write, never to change
  or drop the output.
- **No CI has ever run.** Phase 0 may find problems on Linux that macOS did not show.
- **Security review.** Phases 3 and 4 change the attack surface (a TCP listener, a stdio mode).
  Each needs its threat model entry and its negative tests before it is merged.
