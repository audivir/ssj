# Fallback Transport Specification Proposal: ssj

**Status:** Draft Proposal  
**Context:** `SPEC.md` Issue: Jump hosts (`ProxyJump`), unroutable private networks, and UDP-restricted firewalls.

---

## 1. Problem Statement & Motivation

`ssj` is designed as a secure, roaming-capable shell alternative with:
1. Session persistence across roaming, sleep, and packet loss (like `mosh`).
2. Native local terminal scrollback and streaming bytes (like `et`).
3. Zero root, zero background daemons, zero setuid binaries.
4. UDP transport for speed (QUIC via `quinn` and `rustls`).
5. Memory safety (`#![forbid(unsafe_code)]`).
6. Mutual TLS 1.3 with certificate pinning over the SSH bootstrap channel.

### The Jump Host, NAT Gateway & Blocked UDP Bottleneck

In real-world network architectures, two distinct network topology barriers prevent direct UDP access:

1. **SSH Jump Hosts / Bastions (e.g. `merk-ail-3 -> ProxyJump merk-ail-1`):**
   - The target machine resides on an isolated private subnet (e.g. `192.168.50.3`) reachable only through an intermediate bastion.
   - **OpenSSH `ProxyJump` (`-J` / `ssh -W`) is strictly a TCP stream proxy.** OpenSSH cannot forward or proxy raw UDP datagrams without root-level TUN/TAP virtual network devices (`ssh -w`), violating the zero-root requirement.
2. **NAT Gateways & Routers with Blocked / Unforwarded UDP (e.g. `merk-ail-1` via `141.84.248.11:1162`):**
   - The target host is reachable directly via a public or campus border router that has custom TCP port-forwarding (e.g., external port 1162 forwards to `192.168.0.165:22`).
   - However, **UDP ports (60000–61000) are not forwarded and are dropped by the gateway router firewall**. Even without a jump host, raw UDP to the gateway IP times out completely.
3. **Enterprise & Restrictive Outbound Firewalls:**
   - Many corporate, university, and public Wi-Fi networks block all outbound UDP traffic except DNS (port 53), while permitting outbound TCP on ports 22, 80, and 443.

Under the current v1 specification, `ssj` attempts raw UDP to the destination address. In all three cases above, the initial bootstrap over SSH succeeds, but the subsequent QUIC UDP handshake times out, rendering the session unusable.

This proposal specifies an **in-tree Two-Tier Fallback Transport** for `ssj`. It retains the zero-root, zero-daemon, memory-safe, and mutual-TLS pinned security model of `ssj`, while allowing transparent connectivity across arbitrary SSH jump hosts, NAT gateways with blocked UDP, and locked-down enterprise bastions.

---

## 2. Design Principles

1. **UDP-First:** QUIC over UDP remains the primary transport for direct, high-performance sessions with seamless connection migration. The TCP fallback is strictly activated when UDP is unavailable, unroutable, or when an SSH jump host is detected.
2. **Same Security & Trust Model:** The fallback transport must use the exact same ephemeral certificate pinning established during the SSH bootstrap. No bearer tokens or shared secret keys are introduced. Even when traversing an untrusted jump host or proxy, traffic is end-to-end encrypted with TLS 1.3.
3. **No Root, No Persistent Daemons:** The server remains an unprivileged user process invoked on-demand by the SSH bootstrap.
4. **Transparent Protocol Wire:** The session framing (`ssj-proto`), frame parsing, flow control, and replay buffers (`ReplayBuffer`) remain identical. Only the byte transport changes.
5. **No Slower Than SSH:** Performance on bulk throughput and interactive keystroke latency must be equal to or better than vanilla OpenSSH.

---

## 3. Two-Tier Fallback Transport Architecture

```
                          [ Client Session Start ]
                                     |
                         Can UDP packets reach host?
                                    / \
                            Yes    /   \   No (Jump Host / Blocked UDP)
                                  v     v
              +-----------------------+   +------------------------------------+
              | Primary: QUIC / UDP   |   | Tier 1 Fallback: Ephemeral TCP     |
              | (Direct, fast roaming)|   | via `ssh -W` Bridge (TLS 1.3)     |
              +-----------------------+   +------------------------------------+
                                                             |
                                           Is `AllowTcpForwarding` prohibited?
                                                            / \
                                                    No     /   \   Yes
                                                          v     v
                                                  (Connected)  +------------------------+
                                                               | Tier 2 Final Fallback: |
                                                               | `ssj-stream` via Stdio |
                                                               +------------------------+
```

### 3.1 Tier 1 Primary Fallback: Ephemeral TLS 1.3 over TCP Bridge (`ssh -W`)

When UDP is unreachable (either due to an SSH jump host or an unforwarded UDP gateway like `141.84.248.11:1162`):
1. `ssj-server` dynamically binds an atomic ephemeral TCP listener on `127.0.0.1:0`. The OS kernel assigns an unused port from the full 30,000+ ephemeral range, completely eliminating port squatting and port exhaustion on shared multi-user bastions.
2. The server outputs the dynamically assigned port in its bootstrap line: `SSJ CONNECT <udp_port> tcp:<ephemeral_tcp_port> <fingerprint>`.
3. If connecting via an SSH jump host or port-forwarded gateway, the client spawns an SSH transport worker executing:
   ```shell
   ssh -e none [ssh-options] <host> -W localhost:<ephemeral_tcp_port>
   ```
   (Note: `-e none` is strictly enforced to prevent OpenSSH from intercepting `~.` detach escapes).
   This gives the client a direct, bi-directional raw TCP stream to the server's TCP socket on `localhost` of the target host.
4. The client and server initiate a **mutual TLS 1.3 handshake over the TCP stream** using `rustls` with the identical pinned certificate verifier implemented in `ssj-net`.
5. Once TLS 1.3 is established, standard `ssj-proto` frames (`ClientFrame` and `ServerFrame`) travel across the stream.

**Key Advantages of Tier 1:**
- **Zero new remote processes on reconnect:** Connects straight to the already-running background `ssj-server` socket.
- **Immediate `TCP_NODELAY`:** Raw TCP socket control eliminates buffering delays on interactive keystrokes.
- **Fast reconnect:** Completes in ~1 RTT once the SSH channel is open.
- **Zero Port Collisions / Squatting:** Dynamic `bind(0)` on `127.0.0.1` prevents unprivileged local users from pre-binding or exhausting ports.
- **Chained / Multi-Hop Jumps:** Transparently supports multi-hop jump hosts (e.g. `ProxyJump bastion1,bastion2`) because OpenSSH resolves and traverses the entire bastion chain natively.

### 3.2 Tier 2 Final Fallback: `ssj-stream` over SSH Stdio (The Bulletproof Failsafe)

If the intermediate jump host or enterprise bastion has **`AllowTcpForwarding no`** or strict `PermitOpen` rules configured in `sshd_config`, `ssh -W` will be rejected with:
```text
channel 0: open failed: administratively prohibited: open failed
```
In this scenario, `ssj` automatically drops down to **Tier 2: SSH Stdio Stream**:
1. `ssj-client` spawns a non-interactive SSH command:
   ```shell
   ssh [ssh-options] <host> "sh -c 'exec ~/.local/share/ssj/bin/ssj-server-p1 --stdio --resume <session_id>'"
   ```
2. The client and server exchange `ssj-proto` frames directly across the **standard `stdin`/`stdout` UNIX pipes** of the SSH process.
3. **Zero Bound Ports:** Does not bind any TCP or UDP ports on the remote system (not even on `127.0.0.1`).
4. **Zero Forwarding Privileges Required:** Relies exclusively on standard SSH command execution channels, which are permitted on 100% of hosts where SSH access exists.
5. **Banner / MOTD Discard Guard:** To prevent login banners (`/etc/motd`, legal notices, shell profile warnings) from corrupting binary frames, the server emits a 16-byte synchronization header `b"\x00SSJ_SYNC_STREAM\n"` before the first frame. The client discards any preceding ASCII bytes until this marker is detected.
6. If the connection drops, event-driven reconnect simply re-invokes the command with `--resume <session_id>`, and the server catches up from its in-memory replay buffer.

---

## 4. Bootstrap & Fallback Negotiation

### 4.1 Extended Bootstrap Handshake

Currently, `ssj-server` outputs on stdout during bootstrap:
```text
SSJ CONNECT <port> <fingerprint>
```
To support both UDP and TCP fallback without breaking protocol parsing, the server advertises its capabilities on the connect line:
```text
SSJ CONNECT <udp_port> tcp:<tcp_port> <fingerprint>
```
If UDP cannot bind (or if UDP is disabled via `--no-udp`), the server reports:
```text
SSJ CONNECT none tcp:<tcp_port> <fingerprint>
```

### 4.2 Client Mode Selection Strategy

The client selects the transport based on an automatic discovery cascade:

1. **Pre-flight Jump Detection:**
   The client inspects the target host configuration using `ssh -G <dest>`.
   - If `proxyjump` or `proxycommand` is present, and no explicit `--udp-host` was provided, the client knows raw UDP cannot traverse the jump host. It selects **Tier 1 TCP Fallback (`ssh -W`) immediately** (0 ms discovery delay).
2. **Probing Fallback (Direct Connection / Blocked UDP Gateways):**
   - If no jump host is detected, the client sends the initial QUIC datagram to the target IP.
   - If no response (handshake retry or ack) is received within **1.5 seconds** (e.g. gateway router drops UDP 60000–61000), the client immediately engages **Tier 1 TCP Fallback (`ssh -W`)**.
3. **Automatic Downgrade to Tier 2 (Stdio Stream):**
   - If Tier 1 (`ssh -W`) is rejected by the server/jump host with `administratively prohibited` (i.e. `AllowTcpForwarding no` is enforced), the client immediately invokes **Tier 2 (`ssj-server --stdio`)** as the final bulletproof channel.
4. **CLI Control:**
   - `--transport auto` (default): Use UDP if direct, TCP (`ssh -W`) if jump host or UDP timeout, Stdio if forwarding disabled.
   - `--transport udp`: Force UDP only (fail if UDP unreachable).
   - `--transport tcp`: Force Tier 1 TCP/TLS fallback.
   - `--transport stdio`: Force Tier 2 Stdio stream fallback.

---

## 5. Session Persistence & Reconnection on TCP

On UDP, QUIC handles connection migration transparently when the client IP changes.
On TCP, changing the client's network (e.g. Wi-Fi to LTE) terminates the TCP socket. `ssj` handles this cleanly at the session layer:

1. **Disconnect Detection:**
   The client detects connection loss via TCP socket EOF/RST or when application keepalive (`Ping`) receives no response for 5 seconds (`STALL_AFTER`).
2. **Client Reconnect Backoff:**
   The client enters its existing backoff loop (`0.25s`, `0.5s`, `1s`, `2s`, `3s`).
3. **Re-establishing Transport:**
   For each attempt, the client:
   - Re-opens the TCP bridge (either direct TCP or re-invoking `ssh -W localhost:<tcp_port>`).
   - Completes the mutual TLS 1.3 handshake with the existing session certificate.
   - Transmits `Hello::Resume { output_offset, session_id }`.
4. **Server State Catchup:**
   - The server verifies the pinned certificate.
   - The server validates `output_offset` against its in-memory `ReplayBuffer`.
   - The server replies with `Welcome { input_offset }`.
   - Both sides flush unacknowledged bytes accumulated while offline. No keystrokes or terminal output are dropped or duplicated.

---

## 6. Comparison with Eternal Terminal (`et`)

A common question is: *If `ssj` adds a TCP fallback, has it simply reinvented Eternal Terminal?*

No. Even in fallback mode, `ssj` maintains fundamental architectural advantages:

| Dimension | Eternal Terminal (`et`) | `ssj` (with TCP Fallback) |
| :--- | :--- | :--- |
| **Daemon Requirement** | **Requires root / system daemon (`etserver`)** running continuously on TCP 2022. | **Zero daemons, no root**. Spawns on-demand as unprivileged user via SSH, shuts down when detached/killed. |
| **Primary Transport** | **TCP-only**. Never uses UDP. Suffers from TCP head-of-line blocking on lossy paths. | **UDP-first (QUIC)** for speed, seamless in-flight connection migration, and zero head-of-line blocking. TCP is purely an opportunistic fallback. |
| **Security Architecture** | Transmits and stores a **shared bearer secret key**. Custom framing over protobuf and libsodium. | **Mutual TLS 1.3** with pinned ephemeral certificates. No bearer secret key exists at any stage. |
| **Implementation Safety** | C++ codebase. | **100% memory-safe Rust** (`#![forbid(unsafe_code)]`). |
| **Deployment** | Requires manual installation on remote host, systemd unit configuration, and opening firewall ports. | **Automated bootstrap install** (`ssj install`, `auto_install = "ask"`), pushing static standalone binaries automatically. |

---

## 7. Performance & Benchmark Requirements: "Not Slower Than SSH"

A primary design constraint for `ssj` is that **under no circumstances should `ssj` be slower than OpenSSH** on identical network paths. Adding a TCP/TLS fallback must not compromise throughput or interactive latency.

### 7.1 Performance Targets

| Metric | Target vs OpenSSH | Rationale |
| :--- | :--- | :--- |
| **Bulk Output Throughput** | **$\ge 98\%$ of OpenSSH** | Reading large files (`cat 1GB.log`), streaming logs, or running builds must saturate available link bandwidth without CPU bottlenecks. |
| **Interactive Keystroke Latency** | **$\le +0.5\text{ ms}$ over OpenSSH** | Latency delta on single keystrokes must be imperceptible. |
| **Large Paste Throughput** | **Faster than OpenSSH** on lossy links | 8 MiB paste must not deadlock or stall under 50–100 ms RTT and $1\%$ packet loss. |
| **Memory Footprint** | **$\le 20\text{ MiB}$ RSS** | Single lightweight Tokio async task vs OpenSSH multi-process separation. |
| **CPU Utilization** | **Comparable to OpenSSH** | Hardware-accelerated AES-GCM / ChaCha20-Poly1305 via `ring` assembly. |

### 7.2 Implementation Rules for Performance

1. **Disable Nagle's Algorithm Immediately (`TCP_NODELAY`):**
   On both client and server TCP sockets, `set_nodelay(true)` must be set immediately. Without this, individual interactive keystrokes will be buffered by the OS kernel for up to 40 ms, destroying interactive feel.
2. **Zero-Copy & Direct PTY Buffer Sizing:**
   - Avoid intermediate buffer copying between PTY reads, frame encoding, TLS record encryption, and socket writes.
   - PTY chunk size must dynamically adapt: 1–64 bytes for interactive typing (immediate flush), 32 KiB–64 KiB for continuous streaming output.
3. **TLS Record Sizing:**
   - For interactive traffic: flush small TLS records immediately without waiting for full 16 KiB TLS record blocks.
   - For bulk streaming: pack TLS records to maximum size (16 KiB) to minimize encryption and framing overhead.
4. **Buffer Backpressure Tuning:**
   - Replay buffer and stream windows must ensure backpressure flows from PTY $\rightarrow$ server buffer $\rightarrow$ TCP socket $\rightarrow$ client buffer without deadlocking on simultaneous input paste and burst output.

### 7.3 Automated Benchmark Suite

To enforce this requirement, CI will include an automated benchmark regression harness comparing:
1. `ssh` (OpenSSH default cipher `chacha20-poly1305@openssh.com`)
2. `ssh -J` (OpenSSH over jump host)
3. `ssj (UDP / QUIC)`
4. `ssj (TCP Fallback)`
5. `et` (Eternal Terminal)

#### Benchmark Scenarios:

1. **Bulk Streaming Throughput:**
   - Command: `head -c 100M /dev/urandom | base64` piped through the session.
   - Metric: Megabytes per second (MiB/s) and peak CPU usage.
   - **Gate:** Fails CI if `ssj` throughput is $> 5\%$ slower than OpenSSH on a local 1 Gbps / 10 Gbps loopback.
2. **Interactive Keystroke Round-Trip Latency:**
   - Method: Automated client injects 1,000 sequential keystrokes with pseudo-random inter-keystroke delays (10–100 ms) and measures time-to-echo from a remote PTY in raw mode.
   - Metric: Mean latency, p95 latency, and p99 jitter.
   - **Gate:** Fails CI if `ssj` mean round-trip latency exceeds OpenSSH by $> 1.0\text{ ms}$.
3. **Flaky Network & Packet Loss Stress Test:**
   - Simulated network using `tc-netem` (100 ms RTT, 2% packet loss).
   - Test: 8 MiB paste into a program that echoes output concurrently.
   - Metric: Total completion time and consistency.
   - **Target:** `ssj (UDP)` outperforms OpenSSH by $> 30\%$; `ssj (TCP Fallback)` completes without deadlocks and matches OpenSSH completion time.

---

## 8. Speculative Local Echo: Client-Side Latency Compensation

### 8.1 Prior Art & Real-World Validation
VS Code's integrated terminal and `xterm.js` introduced `terminal.integrated.localEchoLatencyThreshold` in 2020. It has been tested and battle-proven across millions of daily developer sessions on high-latency SSH connections.

Unlike Mosh, which uses an authoritative server-side VT emulator that overwrites screen state and breaks native scrollback, the VS Code / `xterm.js` model operates as a **strictly ephemeral client-side visual overlay**:
1. When round-trip latency exceeds a threshold (e.g. 30 ms), typed printable characters are optimistically drawn to the local terminal in a distinct visual style (e.g. dim, underline, or grayed text via ANSI attributes `\e[2m` or `\e[4m`).
2. The raw wire bytes transmitted to and received from the remote PTY remain 100% unaltered. Native scrollback is completely preserved.
3. If the server output matches the predicted characters, the speculative styling is cleared (confirmed).
4. If the server emits any unexpected bytes, escape sequences, cursor movements, or newlines, the client immediately wipes all speculative characters and paints the authoritative server stream.

### 8.2 The `ssj` Advantage: PTY-Assisted Zero-Heuristic Signaling
VS Code and client-side terminal emulators must guess when a program does not want echo (using regexes on program names or heuristics). In `ssj`, the server controls the PTY and knows the exact terminal line discipline:
- `ssj-server` inspects its PTY flags via `tcgetattr`.
- When password entry occurs (`termios.c_lflag & ECHO == 0`), or when the terminal is in raw/non-canonical mode, `ssj-server` emits a control frame: `ServerFrame::EchoControl(EchoState::Suppressed)`.
- **Full-Screen TUI Protection:** Unix kernel `termios` flags do not capture application-level ANSI `smcup` sequences. In full-screen TUI programs (`vim`, `htop`, `tmux`, `fzf`), speculative echo is completely disabled to avoid ghost characters and cursor drift without requiring an expensive 5,000-line VT emulator. Speculative local echo operates strictly during canonical shell typing (`ICANON && ECHO`).
- Result: Glitch-free latency masking on interactive shell typing over mobile/satellite links, zero risk of password leakage, and zero impact on full-screen TUIs or bulk throughput.

---

## 9. Recommended Quality-of-Life (QOL) Upgrades

Beyond the TCP fallback and speculative echo, several high-impact QOL improvements will elevate `ssj` from a technical proof-of-concept to a premier daily driver for remote development:

### 9.1 Pre-Flight SSH Configuration Resolution (`ssh -G`)
- **Current Limitation:** Passing an SSH alias (like `merk-ail-3`) fails because `ssj-client` resolves aliases via DNS.
- **QOL Solution:** Run `ssh -G <destination>` before connecting to automatically extract `HostName`, `User`, `Port`, and `ProxyJump`.
  - Automatically maps aliases to target IPs/hostnames.
  - Automatically triggers the `ssh -W` TCP fallback when a jump host is present.

### 9.2 Auto-Attach to Last Active Session (`ssj --attach` / `auto_attach`)
- **Current Limitation:** Reconnecting after closing a terminal requires running `ssj list user@host`, copying a random session ID, and running `ssj attach <id> user@host`.
- **QOL Solution:**
  - Running `ssj attach user@host` (with no ID specified) automatically joins the user's running session if exactly one exists.
  - Adding `auto_attach = true` in `client.toml` allows `ssj user@host` to automatically resume an existing detached session instead of spawning a new one.

### 9.3 Fast Wake-from-Sleep Reconnect Trigger
- **Current Limitation:** When a laptop wakes from sleep, `ssj-client` waits for its 5-second `STALL_AFTER` ping timer and subsequent backoff before realizing the path is dead and attempting reconnect.
- **QOL Solution:** Hook OS power notifications (`IOPMrootDomain` on macOS, `systemd-logind` on Linux) to pause keepalives before sleep and trigger an immediate, aggressive reconnect probe the moment the system wakes up and network connectivity is restored.

### 9.4 SSH Agent Forwarding (`SSH_AUTH_SOCK`)
- **Current Limitation:** Pushing code via Git from a remote `ssj` session fails unless credentials or keys are copied to the remote host.
- **QOL Solution:** Multiplex a local Unix domain socket stream for `SSH_AUTH_SOCK` over a dedicated QUIC/TLS stream in `ssj-net`. This allows using local SSH keys remotely without storing private keys on the server.

### 9.5 Comprehensive OSC (Operating System Command) Terminal Integration

Modern terminal emulators (Ghostty, WezTerm, iTerm2, Alacritty, Kitty, Foot) use Operating System Command (OSC) escape sequences (`ESC ] <num> ; <params> ST`) to bridge remote CLI applications with local desktop OS features. Supporting and managing these sequences cleanly provides huge QOL benefits:

| OSC Sequence | Name & Syntax | Purpose & Value for `ssj` |
| :--- | :--- | :--- |
| **OSC 0 / 1 / 2** | Window & Tab Titles<br>`\e]0;title\a` | Sets local terminal window and tab titles. `ssj` already uses the xterm title stack (`CSI 22;0t` / `CSI 23;0t`) for connection warnings; preserving client OSC 0/2 ensures remote shell prompts (e.g. Starship, Oh-My-Zsh) can name the tab. |
| **OSC 7** | Working Directory (CWD)<br>`\e]7;file://host/path\a` | Communicates remote directory changes. Allows local terminal "Duplicate Tab" / "Split Pane" shortcuts to open directly in the remote host's current directory. |
| **OSC 8** | Clickable Hyperlinks<br>`\e]8;;url\a text \e]8;;\a` | Allows remote compilers (`rustc`), `git`, and tools to emit rich clickable URLs and documentation links without cluttering terminal text with raw URLs. |
| **OSC 9 / OSC 777** | Desktop Notifications<br>`\e]777;notify;title;body\a`<br>`\e]9;msg\a` | Emits native macOS/Linux desktop notifications when long remote jobs finish (`cargo build`, `pytest`, `training.py`), even if the user has switched windows. |
| **OSC 10 / 11 / 12** | Theme & Color Palette Query<br>`\e]11;?\a` | Queries foreground, background, and cursor colors. Used by Neovim, `fzf`, `bat`, and `delta` to auto-detect Dark vs Light theme. *Note:* In high-latency sessions, `ssj` can cache the local terminal's response on connect to answer remote queries instantly without round-trip stalls. |
| **OSC 52** | Clipboard Integration<br>`\e]52;c;<base64>\a` | Enables copy-to-clipboard from remote CLI tools (`tmux`, `vim`, `pbcopy` wrappers) to local clipboard without X11 or Wayland forwarding. |
| **OSC 133** | Semantic Prompt Shell Integration<br>`\e]133;A\a` (Prompt), `\e]133;C\a` (Output) | FinalTerm/FTCS prompt marking. Enables jumping between command prompts with Cmd+Up/Down, copying only command output, and visual command status indicators. |
| **OSC 1337** | Terminal Badges & Inline Images<br>`\e]1337;SetBadgeFormat=...\a` | iTerm2/Ghostty extension. Allows rendering a faint watermark badge in the terminal background (e.g., displaying `[ssj: merk-ail-3]` or `PROD`) so the user never forgets they are typing in a remote production shell. |

### 9.6 1:1 Match with OpenSSH: Transparent Pass-Through

In alignment with OpenSSH and [SPEC.md](file:///Users/tim/Repos/sss/SPEC.md#L511), `ssj` adopts the **dumb pipe philosophy**:
- **Zero In-Stream Parsing:** `ssj` performs no inspection, filtering, or parsing of terminal escape sequences on the data path. Bytes received from the server are written directly to `stdout` (`io.output.write_all(&data)`).
- **Separation of Concerns:** Terminal security (such as disallowing clipboard read queries, restricting answerback responses, and prompting on untrusted hyperlinks) is the strict responsibility of the local terminal emulator (Ghostty, iTerm2, Alacritty, WezTerm), which already implements these protections natively.
- **Benefits:**
  1. **Maximum Speed:** Zero CPU overhead on bulk output, matching OpenSSH throughput.
  2. **Zero Attack Surface:** No escape sequence parser in `ssj-client` that could be exploited by malicious remote output.
  3. **Universal Compatibility:** Any new escape sequences supported by modern terminals work out of the box without needing updates to `ssj`.

### 9.7 CLI Modernization with `clap`, SSH Destination Parsing & Autocomplete Build Script

To provide standard CLI ergonomic parity with OpenSSH, `ssj-client` will modernize its command-line interface using `clap` (v4 with derive) and an automated `build.rs` asset generator:

1. **Full OpenSSH `user@host` Destination Parsing:**
   - Rather than naive string splitting, the destination argument is validated and parsed by a robust `Destination` type matching OpenSSH syntax:
     - `host` (e.g. `merk-ail-1` $\rightarrow$ automatically inherits current user or `User` from `~/.ssh/config`).
     - `user@host` (e.g. `tihoph@192.168.0.165`).
     - `user@host:port` or `host:port` (custom SSH/bootstrap port).
     - `ssh://[user@]host[:port]` (standard SSH URI notation).
     - IPv6 literals: `user@[2001:db8::1]:port`.
     - **Strict Option Injection Guard:** Rejects any input starting with `-` (preventing option injection into the underlying `ssh` bootstrap).
2. **Dynamic SSH Host Tab-Completion (Matching `ssh <TAB>`):**
   - The destination argument is decorated with `#[arg(value_hint = clap::ValueHint::Hostname)]`.
   - When generating shell completions:
     - **Zsh:** Hooks into `_hosts` / `_ssh_hosts`. Automatically tab-completes all `Host` entries from `~/.ssh/config` (aliases like `merk-ail-1`, `merk-ail-3`), `~/.ssh/known_hosts`, and `/etc/hosts`, including dynamic completion after typing a `user@` prefix.
     - **Bash:** Hooks into `_known_hosts_real` / `compgen -A hostname`.
     - **Fish:** Hooks into native `__fish_print_hostnames`.
3. **Automated `build.rs` Completion & Man Page Generation:**
   - Add a `build.rs` build script to `crates/ssj-client/` with `build-dependencies`: `clap`, `clap_complete`, and `clap_mangen`.
   - Automatically generates:
     - Shell completions for **Bash, Zsh, and Fish** in `target/assets/completions/`.
     - Section 1 roff **man page (`ssj.1`)** in `target/assets/man/`.
   - **Exclusively `.tar.gz` Distribution Archives:** Releases produce only standardized `.tar.gz` archives (`ssj-v<version>-<target>.tar.gz`) containing the compiled binaries, man pages, shell completion scripts, `README.md`, `LICENSE`, and `SHA256SUMS`. No `.deb`, `.rpm`, or platform-specific package formats will be built or published.

### 9.8 Human-Readable Session Naming (`--name` / `-N`)

Currently, sessions are identified only by random 16-character base32 IDs (e.g. `abcdef012345`). Remembering and typing random IDs hinders workflow:

1. **Named Session Creation:**
   - Add `--name <name>` / `-N <name>` to `ssj [user@]host`:
     ```shell
     ssj --name work user@host
     ```
   - Names allow alphanumeric characters, hyphens, and underscores (`^[a-zA-Z0-9_-]{1,32}$`).
2. **Attaching and Killing by Name:**
   - Users can attach or kill by name or ID interchangeably:
     ```shell
     ssj attach work user@host
     ssj kill work user@host
     ```
3. **Auto-Attach by Name (Tmux-Style `-A` Semantics):**
   - Running `ssj --name work user@host` automatically checks if a session named `work` is already running/detached. If found, it attaches immediately; if not, it spawns a new session with that name.
4. **User Isolation & Namespacing:**
   - Sessions are stored and managed per-user in the user's isolated `$XDG_RUNTIME_DIR/ssj/` or `$XDG_STATE_HOME/ssj/`. Multiple independent users on a shared multi-tenant server can each have a session named `work` without collision or access leakage.

### 9.9 Asynchronous Background-Cached Tab Completion (`ssj attach <host> <TAB>`)

Querying remote sessions during shell tab-completion presents a major latency bottleneck: executing `ssh user@host "ssj list"` synchronously freezes the shell for 300–800 ms every time the user hits `<TAB>`.

To deliver **0 ms instant tab-completion**, `ssj` implements local caching with non-blocking background refreshes:

1. **Local Session Cache:**
   - Session states are cached per host under `$XDG_CACHE_HOME/ssj/sessions/<host>.json` (with session IDs, custom names, age, and state).
2. **Instant Tab Response:**
   - When the user types `ssj attach <host> <TAB>` or `ssj kill <host> <TAB>`, the shell completion script reads the local cache file immediately, displaying available session names and IDs in $< 2\text{ ms}$ with zero network delay.
3. **Non-Blocking Background Refresh:**
   - If the cache is older than TTL (e.g. 60 seconds) or missing, the completion script launches a detached, non-blocking background subshell:
     ```shell
     (ssj list "$host" --json > "$cache_tmp" && mv "$cache_tmp" "$cache_file") >/dev/null 2>&1 &!
     ```
   - The current tab press completes instantly from the available cache; subsequent tab presses automatically reflect the refreshed background data.
4. **Ambient Cache Invalidation:**
   - Every normal user command (`ssj`, `ssj list`, `ssj attach`, `ssj kill`) automatically updates the local host cache upon completion.

---

## 10. Migration & Implementation Plan

1. **Crate Updates:**
   - `ssj-proto`: Add optional `tcp_port: Option<u16>` to `ssj_proto::bootstrap::Connect`, and support optional human-readable `name` in session metadata. The wire protocol integer `PROTOCOL` is bumped to `2`, accompanied by the new golden file `crates/ssj-proto/tests/golden/protocol-2.bin` and an update to `PROTOCOLS.md`.
   - `ssj-net`: Abstract transport behind a common `SessionStream` interface delivering authenticated bidirectional frame I/O over either `quinn::Connection` or `tokio_rustls::TlsStream<TcpStream>`.
   - `ssj-server`: Add optional `--tcp-port-range` or dual-bind listener in `listen.rs`, plus `--stdio` mode and `--name` storage.
   - `ssj-client`: 
     - Integrate `clap` (v4 derive) and implement the SSH-compatible `Destination` parser and `--name` option.
     - Add `build.rs` to generate Bash/Zsh/Fish completions and the `ssj.1` man page.
     - Implement pre-flight `ssh -G` jump detection and automatic `ssh -W` bridge invocation in `run.rs`.
     - Implement local session caching under `$XDG_CACHE_HOME/ssj/sessions/` for instant async tab-completion.
2. **Packaging & Testing:**
   - Release workflow packages exclusively `.tar.gz` archives per platform containing binaries, man pages, completions, and checksums.
   - Golden wire format test verified with `protocol-2.bin`.
   - Unit tests for dual-stack bootstrap parsing, SSH destination syntax, and session name validation.
   - Integration tests verifying seamless fallback over a simulated jump host.
   - CI automated throughput and latency benchmarks.




