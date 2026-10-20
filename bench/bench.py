#!/usr/bin/env python3
# ruff: noqa: T201
"""Compares ssh with ssj over UDP, TCP, and the ssh channel.

It measures bulk output, keystroke latency, and a large paste. Each client runs on a pseudo
terminal, as a user would run it.

    bench/bench.py --host me@localhost --ssj target/release/ssj \
        --server target/release/ssj-server [--netem "delay 40ms loss 1%"] [--json]

--train replaces the measurements with a ride on a train: the link loses packets in bursts all the
time (give it with --netem or NETEM=train of docker-bench.sh), and --outages seconds-long blackouts,
in which every packet is dropped in both directions while the interface stays up, are cut into the
typing. It reports the keystroke latency outside the outages and the time from the end of each
outage to the first echo. Blackouts need Linux, root, and iptables.

--netem needs Linux and root (it runs `tc qdisc` on the loopback device) and applies to ssh and
ssj alike, so use it with --host localhost. Nothing is gated: the script prints numbers.
"""

from __future__ import annotations

import argparse
import contextlib
import itertools
import json
import os
import pty
import random
import select
import shlex
import statistics
import subprocess
import sys
import time
from pathlib import Path
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from collections.abc import Sequence

MARK = "END" + "42"  # the command shows END$((6*7)), the output shows END42


class Args(argparse.Namespace):
    """The command line, with the type of every option."""

    host: str
    ssj: str
    server: str
    ssh_command: str
    congestion: str | None
    only: str | None
    text_bulk: bool
    allow_drop: bool
    bulk_mb: int
    raw_bulk: bool
    paste_mb: int
    samples: int
    jitter_ms: int
    rounds: int
    startup: int | None
    train: bool
    outages: str
    quiet: float
    netem: str | None
    json: bool


class Terminal:
    """A client process on a pseudo terminal."""

    def __init__(self, argv: Sequence[str]) -> None:
        """Starts `argv` on a new pseudo terminal."""
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.execvp(argv[0], list(argv))
        self.buffer = b""
        self.seen = b""

    def send(self, data: str | bytes) -> None:
        """Writes `data` to the terminal, and reads what the client prints meanwhile."""
        if isinstance(data, str):
            data = data.encode()
        view = memoryview(data)
        while view:
            # keep reading while writing, so that a full output queue cannot block the paste
            readable, writable, _ = select.select([self.fd], [self.fd], [], 5)
            if readable:
                self.read_some()
            if writable:
                view = view[os.write(self.fd, view[:65536]) :]

    def read_some(self) -> bytes:
        """Reads what the client printed, and answers a request for the cursor position."""
        try:
            chunk = os.read(self.fd, 1 << 20)
        except OSError:
            chunk = b""
        # a terminal answers a request for the cursor position, and busybox `ash` waits for it
        for _ in range(chunk.count(b"\x1b[6n")):
            os.write(self.fd, b"\x1b[24;80R")
        self.buffer += chunk
        self.seen = (self.seen + chunk)[-300:]
        return chunk

    def wait_for(self, text: str, timeout: float = 300) -> int:
        """Reads until `text` is printed. Returns the number of bytes before it."""
        needle = text.encode()
        end = time.monotonic() + timeout
        total = 0
        while True:
            at = self.buffer.find(needle)
            if at >= 0:
                total += at
                self.buffer = self.buffer[at + len(needle) :]
                return total
            # keep a tail so that a marker across two reads is found
            keep = len(needle) - 1
            total += max(0, len(self.buffer) - keep)
            self.buffer = self.buffer[-keep:] if keep else b""
            if time.monotonic() > end:
                raise TimeoutError(f"no {text!r}, the last output was {self.seen[-300:]!r}")
            readable, _, _ = select.select([self.fd], [], [], 1)
            if readable and not self.read_some():
                raise EOFError("the client ended")

    def close(self) -> None:
        """Ends the shell, and waits for the client to end (or kills it)."""
        with contextlib.suppress(OSError):
            os.write(self.fd, b"\nexit\n")
        # a process cannot end while its output is queued on the terminal, so keep reading
        killed = False
        end = time.monotonic() + 5
        while True:
            done, _ = os.waitpid(self.pid, os.WNOHANG)
            if done:
                break
            if not killed and time.monotonic() > end:
                os.kill(self.pid, 9)
                killed = True
            readable, _, _ = select.select([self.fd], [], [], 0.05)
            if readable and not self.read_some():
                time.sleep(0.05)
        self.buffer = b""
        os.close(self.fd)


def clients(args: Args) -> dict[str, list[str]]:
    """The command line of every client."""
    base = shlex.split(args.ssh_command)
    ssj = [args.ssj, "--server-path", args.server, "--ssh-command", args.ssh_command]
    if not args.allow_drop:
        # every byte must arrive for the numbers to mean the same for every client
        ssj += ["--server-arg", "--slow-client", "--server-arg", "wait"]
    if args.congestion:
        ssj += ["--congestion", args.congestion]
    compress = ["--compress", "auto"]
    return {
        "ssh": [*base, "-tt", "-o", "BatchMode=yes", args.host],
        "ssh-C": [*base, "-tt", "-C", "-o", "BatchMode=yes", args.host],
        "udp": [*ssj, "--transport", "udp", args.host],
        "udp-z": ssj + compress + ["--transport", "udp", args.host],
        "tcp": [*ssj, "--transport", "tcp", args.host],
        "tcp-z": [*ssj, *compress, "--transport", "tcp", args.host],
        "stdio": [*ssj, "--transport", "stdio", args.host],
        "stdio-z": [*ssj, *compress, "--transport", "stdio", args.host],
    }


def session(argv: Sequence[str]) -> Terminal:
    """Starts a client and returns it when the shell answers."""
    terminal = Terminal(argv)
    # the shell is ready when it answers a command
    terminal.send("echo READY$((6*7))\n")
    terminal.wait_for("READY42", 60)
    return terminal


TEXT_LINE = "2026-10-18 12:00:%02d INFO worker-%d handled request id=%d status=200 bytes=%d"


def bulk(
    argv: Sequence[str], megabytes: int, raw: bool = False, text: bool = False
) -> dict[str, Any]:
    """Seconds for the shell to print `megabytes` of output."""
    terminal = session(argv)
    if text:
        # lines like a log, which compress about the way build and server output does
        lines = megabytes * 1048576 // 80
        terminal.send(
            f"awk 'BEGIN {{ for (i = 0; i < {lines}; i++) "
            f'printf "{TEXT_LINE}\\n", i % 60, i % 8, (i * 7919) % 100000, (i * 31) % 5000 }}\''
            f"; echo {MARK[:3]}$((6*7))\n"
        )
    else:
        # `tr` makes text of the zeros, and it is slower than the transports on some systems, so
        # the numbers would show `tr`. With `raw` the zeros go to the terminal as they are.
        producer = "" if raw else " | tr '\\0' a"
        terminal.send(
            f"dd if=/dev/zero bs=1048576 count={megabytes} 2>/dev/null{producer};"
            f" echo {MARK[:3]}$((6*7))\n"
        )
    start = time.monotonic()
    terminal.wait_for(MARK)
    seconds = time.monotonic() - start
    terminal.close()
    return {"megabytes_per_second": round(megabytes / seconds, 1), "seconds": round(seconds, 2)}


def latency(argv: Sequence[str], samples: int, jitter_ms: int = 0) -> dict[str, Any]:
    """The time from a keystroke to its echo."""
    terminal = session(argv)
    terminal.send("stty raw -echo; cat\n")
    time.sleep(0.5)
    terminal.buffer = b""
    times = []
    for index in range(samples):
        byte = bytes([ord("a") + index % 26])
        start = time.monotonic()
        terminal.send(byte)
        while byte not in terminal.buffer:
            readable, _, _ = select.select([terminal.fd], [], [], 5)
            if not readable or not terminal.read_some():
                raise TimeoutError("no echo")
        times.append((time.monotonic() - start) * 1000)
        terminal.buffer = b""
        # a gap that varies, so that keystrokes meet the pings and acknowledgements at random
        time.sleep(0.01 + random.random() * jitter_ms / 1000)
    os.write(terminal.fd, b"\x04")
    terminal.close()
    times.sort()
    return {
        "median_ms": round(statistics.median(times), 2),
        "p95_ms": round(times[int(len(times) * 0.95) - 1], 2),
        "max_ms": round(times[-1], 2),
        "samples": samples,
        # echoes that took more than 10 ms over the median: a stall and not a slow link
        "stalls": sum(1 for t in times if t > statistics.median(times) + 10),
    }


def paste(argv: Sequence[str], megabytes: int, text: bool = False) -> dict[str, Any]:
    """Seconds for the shell to take `megabytes` of input."""
    terminal = session(argv)
    size = megabytes * 1048576
    terminal.send(f"stty raw -echo; head -c {size} > /dev/null; echo {MARK[:3]}$((6*7))\n")
    time.sleep(0.5)
    terminal.buffer = b""
    start = time.monotonic()
    if text:
        lines = [
            (TEXT_LINE % (i % 60, i % 8, (i * 7919) % 100000, (i * 31) % 5000)) + "\n"
            for i in range(size // 80 + 1)
        ]
        payload = "".join(lines).encode()[:size]
    else:
        payload = b"x" * size
    terminal.send(payload)
    terminal.wait_for(MARK)
    seconds = time.monotonic() - start
    terminal.close()
    return {"seconds": round(seconds, 2), "megabytes_per_second": round(megabytes / seconds, 1)}


def startup(argv: Sequence[str], runs: int) -> dict[str, Any]:
    """Seconds from starting the client until the shell answers a command, and until it is gone."""
    ready, ended = [], []
    for _ in range(runs):
        begin = time.monotonic()
        terminal = session(argv)
        ready.append(time.monotonic() - begin)
        begin = time.monotonic()
        terminal.close()
        ended.append(time.monotonic() - begin)
    return {
        "ready_s": round(statistics.median(ready), 2),
        "exit_s": round(statistics.median(ended), 2),
    }


def blackout(on: bool) -> None:
    """Drops every packet of eth0 in both directions, or lets them pass again.

    The interface and its address stay, as when a train enters a tunnel: nothing tells the
    programs that the network is gone.
    """
    action = "-I" if on else "-D"
    for chain, flag in (("INPUT", "-i"), ("OUTPUT", "-o")):
        subprocess.run(["iptables", "-w", action, chain, flag, "eth0", "-j", "DROP"], check=False)


def type_and_wait(terminal: Terminal, byte: bytes, timeout: float = 60) -> float | None:
    """Types a byte and returns the milliseconds until its echo, or None if none came."""
    start = time.monotonic()
    terminal.send(byte)
    while byte not in terminal.buffer:
        left = timeout - (time.monotonic() - start)
        if left <= 0:
            return None
        readable, _, _ = select.select([terminal.fd], [], [], min(left, 1))
        if readable and not terminal.read_some():
            return None
    terminal.buffer = b""
    return (time.monotonic() - start) * 1000


LETTERS = itertools.cycle(bytes([c]) for c in range(ord("a"), ord("z") + 1))
# the longest wait for a sign of life after a blackout
RECOVERY_LIMIT = 120
# bytes that no status message and no letter of the typing contains
MARKER = b"\x01\x02\x03\x04"


def type_for(terminal: Terminal, seconds: float, times: list[float]) -> int:
    """Types at the pace of a person for `seconds`. Returns the number of lost echoes."""
    lost = 0
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        took = type_and_wait(terminal, next(LETTERS))
        if took is None:
            lost += 1
        else:
            times.append(took)
        time.sleep(0.05 + random.random() * 0.15)
    return lost


def go_through_blackout(terminal: Terminal, seconds: int) -> float | None:
    """Types during a blackout, and returns the seconds from its end to the echo of a marker."""
    blackout(True)
    try:
        # the user goes on typing, and the keys wait in the client.
        for _ in range(3):
            terminal.send(next(LETTERS))
            time.sleep(seconds / 3)
    finally:
        blackout(False)
    # the marker is typed when the network is back, and its echo is the sign of life. Anything
    # else the client prints meanwhile (a status in the window title) does not count.
    terminal.buffer = b""
    began = time.monotonic()
    terminal.send(MARKER)
    while time.monotonic() - began < RECOVERY_LIMIT:
        if MARKER in terminal.buffer:
            return round(time.monotonic() - began, 2)
        readable, _, _ = select.select([terminal.fd], [], [], 0.2)
        if readable and not terminal.read_some():
            return None
    return None


def train(argv: Sequence[str], outages: Sequence[int], quiet: float) -> dict[str, Any]:
    """Types for `quiet` seconds, then goes through a blackout, for each of the `outages`."""
    terminal = session(argv)
    terminal.send("stty raw -echo; cat\n")
    time.sleep(0.5)
    terminal.buffer = b""
    times: list[float] = []
    lost = 0
    recoveries: list[dict[str, Any]] = []
    for seconds in outages:
        lost += type_for(terminal, quiet, times)
        recovery = go_through_blackout(terminal, seconds)
        recoveries.append({"outage_s": seconds, "recovery_s": recovery})
        if recovery is None:
            break
        time.sleep(0.5)
        terminal.buffer = b""
    terminal.close()
    times.sort()
    result: dict[str, Any] = {"outages": recoveries, "lost_echoes": lost, "samples": len(times)}
    if times:
        result["median_ms"] = round(statistics.median(times), 2)
        result["p95_ms"] = round(times[max(0, int(len(times) * 0.95) - 1)], 2)
        result["max_ms"] = round(times[-1], 2)
    return result


def print_train(results: dict[str, Any]) -> None:
    """Prints the results of --train as a table."""
    print(
        f"{'':8} {'median ms':>10} {'p95 ms':>8} {'max ms':>8} {'lost':>5}"
        "  seconds from the end of an outage to the first echo"
    )
    for name, r in results.items():
        ends = "  ".join(
            f"{o['outage_s']}s: {'never' if o['recovery_s'] is None else o['recovery_s']}"
            for o in r["outages"]
        )
        print(
            f"{name:8} {r.get('median_ms', '-'):>10} {r.get('p95_ms', '-'):>8}"
            f" {r.get('max_ms', '-'):>8} {r['lost_echoes']:>5}  {ends}"
        )


def netem(args: Args, action: str) -> None:
    """Adds or removes the simulated link on the loopback device."""
    if not args.netem:
        return
    command = ["tc", "qdisc", action, "dev", "lo", "root"]
    if action == "add":
        command += ["netem", *args.netem.split()]
    # root (a container) needs no sudo
    prefix = [] if os.geteuid() == 0 else ["sudo", "-n"]
    subprocess.run(prefix + command, check=False)


def measure(argv: Sequence[str], args: Args) -> dict[str, Any]:
    """Runs the measurements that the options ask for on one client."""
    if args.startup:
        return startup(argv, args.startup)
    if args.train:
        return train(argv, [int(part) for part in args.outages.split(",")], args.quiet)
    rounds = [bulk(argv, args.bulk_mb, args.raw_bulk, args.text_bulk) for _ in range(args.rounds)]
    pastes = [paste(argv, args.paste_mb, args.text_bulk) for _ in range(args.rounds)]
    return {
        "bulk": max(rounds, key=lambda r: r["megabytes_per_second"]),
        "latency": latency(argv, args.samples, args.jitter_ms),
        "paste": max(pastes, key=lambda r: r["megabytes_per_second"]),
    }


def main() -> None:
    """Runs the measurements and prints them."""
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--host", default="localhost")
    parser.add_argument("--ssj", default="target/release/ssj")
    parser.add_argument("--server", default="target/release/ssj-server")
    parser.add_argument("--ssh-command", default="ssh")
    parser.add_argument("--congestion", choices=["bbr", "cubic"], help="for ssj (default: its own)")
    parser.add_argument(
        "--only", help="comma separated: ssh,ssh-C,udp,udp-z,tcp,tcp-z,stdio,stdio-z"
    )
    parser.add_argument("--text-bulk", action="store_true", help="log-like text for bulk and paste")
    parser.add_argument(
        "--allow-drop", action="store_true", help="let ssj skip output on a slow link"
    )
    parser.add_argument("--bulk-mb", type=int, default=256)
    parser.add_argument("--raw-bulk", action="store_true", help="bulk output without tr")
    parser.add_argument("--paste-mb", type=int, default=8)
    parser.add_argument("--samples", type=int, default=300)
    parser.add_argument(
        "--jitter-ms", type=int, default=0, help="random extra gap between keystrokes"
    )
    parser.add_argument(
        "--rounds", type=int, default=3, help="the best bulk and paste round counts"
    )
    parser.add_argument(
        "--startup", type=int, metavar="RUNS", help="only time the start and the end, RUNS times"
    )
    parser.add_argument("--train", action="store_true", help="a ride with blackouts (see above)")
    parser.add_argument(
        "--outages", default="5,15", help="seconds of the blackouts of --train, comma separated"
    )
    parser.add_argument(
        "--quiet", type=float, default=10, help="seconds of typing before each blackout"
    )
    parser.add_argument("--netem", help='for example "delay 40ms loss 1%%" (Linux, root)')
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args(namespace=Args())
    args.ssj, args.server = str(Path(args.ssj).resolve()), str(Path(args.server).resolve())

    chosen = clients(args)
    if args.only:
        chosen = {name: chosen[name] for name in args.only.split(",")}
    results = {}
    netem(args, "add")
    try:
        for name, argv in chosen.items():
            print(f"{name} ...", file=sys.stderr)
            results[name] = measure(argv, args)
    finally:
        netem(args, "del")
    if args.json:
        print(json.dumps(results, indent=2))
        return
    if args.startup:
        print(f"{'':6} {'ready s':>8} {'exit s':>8}")
        for name, r in results.items():
            print(f"{name:6} {r['ready_s']:>8} {r['exit_s']:>8}")
        return
    if args.train:
        print_train(results)
        return
    print(
        f"{'':6} {'bulk MB/s':>10} {'median ms':>10} {'p95 ms':>8} {'max ms':>8}"
        f" {'stalls':>7} {'paste s':>8}"
    )
    for name, r in results.items():
        print(
            f"{name:6} {r['bulk']['megabytes_per_second']:>10} {r['latency']['median_ms']:>10}"
            f" {r['latency']['p95_ms']:>8} {r['latency']['max_ms']:>8} {r['latency']['stalls']:>7}"
            f" {r['paste']['seconds']:>8}"
        )


if __name__ == "__main__":
    main()
