#!/usr/bin/env python3
"""Compares ssh with ssj over UDP, TCP, and the ssh channel: bulk output, keystroke latency,
and a large paste. Each client runs on a pseudo terminal, as a user would run it.

    bench/bench.py --host me@localhost --ssj target/release/ssj \
        --server target/release/ssj-server [--netem "delay 40ms loss 1%"] [--json]

--netem needs Linux and root (it runs `tc qdisc` on the loopback device) and applies to ssh and
ssj alike, so use it with --host localhost. Nothing is gated: the script prints numbers.
"""

import argparse
import json
import os
import pty
import select
import statistics
import subprocess
import sys
import time

MARK = "END" + "42"  # the command shows END$((6*7)), the output shows END42


class Terminal:
    """A client process on a pseudo terminal."""

    def __init__(self, argv):
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.execvp(argv[0], argv)
        self.buffer = b""

    def send(self, data):
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

    def read_some(self):
        try:
            chunk = os.read(self.fd, 1 << 20)
        except OSError:
            chunk = b""
        self.buffer += chunk
        return chunk

    def wait_for(self, text, timeout=300):
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
                raise TimeoutError(f"no {text!r}")
            readable, _, _ = select.select([self.fd], [], [], 1)
            if readable and not self.read_some():
                raise EOFError("the client ended")

    def close(self):
        try:
            os.write(self.fd, b"\nexit\n")
        except OSError:
            pass
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


def clients(args):
    ssj = [args.ssj, "--server-path", args.server, "--ssh-command", args.ssh_command]
    return {
        "ssh": ["ssh", "-tt", "-o", "BatchMode=yes", args.host],
        "udp": ssj + ["--transport", "udp", args.host],
        "tcp": ssj + ["--transport", "tcp", args.host],
        "stdio": ssj + ["--transport", "stdio", args.host],
    }


def session(argv):
    terminal = Terminal(argv)
    # the shell is ready when it answers a command
    terminal.send("echo READY$((6*7))\n")
    terminal.wait_for("READY42")
    return terminal


def bulk(argv, megabytes, raw=False):
    terminal = session(argv)
    # `tr` makes text of the zeros, and it is slower than the transports on some systems, so the
    # numbers would show `tr`. With `raw` the zeros go to the terminal as they are.
    producer = "" if raw else " | tr '\\0' a"
    terminal.send(
        f"dd if=/dev/zero bs=1048576 count={megabytes} 2>/dev/null{producer}; echo {MARK[:3]}$((6*7))\n"
    )
    start = time.monotonic()
    terminal.wait_for(MARK)
    seconds = time.monotonic() - start
    terminal.close()
    return {"megabytes_per_second": round(megabytes / seconds, 1), "seconds": round(seconds, 2)}


def latency(argv, samples):
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
        time.sleep(0.01)
    os.write(terminal.fd, b"\x04")
    terminal.close()
    times.sort()
    return {
        "median_ms": round(statistics.median(times), 2),
        "p95_ms": round(times[int(len(times) * 0.95) - 1], 2),
        "max_ms": round(times[-1], 2),
    }


def paste(argv, megabytes):
    terminal = session(argv)
    size = megabytes * 1048576
    terminal.send(f"stty raw -echo; head -c {size} > /dev/null; echo {MARK[:3]}$((6*7))\n")
    time.sleep(0.5)
    terminal.buffer = b""
    start = time.monotonic()
    terminal.send(b"x" * size)
    terminal.wait_for(MARK)
    seconds = time.monotonic() - start
    terminal.close()
    return {"seconds": round(seconds, 2), "megabytes_per_second": round(megabytes / seconds, 1)}


def netem(args, action):
    if not args.netem:
        return
    command = ["tc", "qdisc", action, "dev", "lo", "root"]
    if action == "add":
        command += ["netem"] + args.netem.split()
    subprocess.run(["sudo", "-n"] + command, check=False)


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--host", default="localhost")
    parser.add_argument("--ssj", default="target/release/ssj")
    parser.add_argument("--server", default="target/release/ssj-server")
    parser.add_argument("--ssh-command", default="ssh")
    parser.add_argument("--only", help="comma separated: ssh,udp,tcp,stdio")
    parser.add_argument("--bulk-mb", type=int, default=256)
    parser.add_argument("--raw-bulk", action="store_true", help="bulk output without tr")
    parser.add_argument("--paste-mb", type=int, default=8)
    parser.add_argument("--samples", type=int, default=300)
    parser.add_argument("--rounds", type=int, default=3, help="the best bulk and paste round counts")
    parser.add_argument("--netem", help='for example "delay 40ms loss 1%%" (Linux, root)')
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    args.ssj, args.server = os.path.abspath(args.ssj), os.path.abspath(args.server)

    chosen = clients(args)
    if args.only:
        chosen = {name: chosen[name] for name in args.only.split(",")}
    results = {}
    netem(args, "add")
    try:
        for name, argv in chosen.items():
            print(f"{name} ...", file=sys.stderr)
            rounds = [bulk(argv, args.bulk_mb, args.raw_bulk) for _ in range(args.rounds)]
            pastes = [paste(argv, args.paste_mb) for _ in range(args.rounds)]
            results[name] = {
                "bulk": max(rounds, key=lambda r: r["megabytes_per_second"]),
                "latency": latency(argv, args.samples),
                "paste": max(pastes, key=lambda r: r["megabytes_per_second"]),
            }
    finally:
        netem(args, "del")
    if args.json:
        print(json.dumps(results, indent=2))
        return
    print(f"{'':6} {'bulk MB/s':>10} {'median ms':>10} {'p95 ms':>8} {'max ms':>8} {'paste s':>8}")
    for name, r in results.items():
        print(
            f"{name:6} {r['bulk']['megabytes_per_second']:>10} {r['latency']['median_ms']:>10}"
            f" {r['latency']['p95_ms']:>8} {r['latency']['max_ms']:>8} {r['paste']['seconds']:>8}"
        )


if __name__ == "__main__":
    main()
