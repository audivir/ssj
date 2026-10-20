#!/usr/bin/env python3
# ruff: noqa: T201
"""Measures what `ssh -W` does to two small writes in a row, without ssj.

    wprobe.py server PORT GAP_US NODELAY MODE    answers every byte with 27 bytes and then 41 bytes,
                                                 GAP_US microseconds apart
    wprobe.py client HOST PORT SAMPLES MODE      reaches the server through `ssh -W`, sends a byte,
                                                 and times the second answer after the first

MODE may start with "up-" (up-plain, up-merge, up-pad2, up-pad1, up-reply): then the client
sends the two writes and the server reports how long the second took to arrive after the first.

MODE is plain; merge (one write of both answers); pad2 (the second answer padded to 2000 bytes);
pad1 (the first answer padded to 2000 bytes); reply (the client sends a byte as soon as it has the
first answer, and the server sends the second one after that byte).

Run by bench/docker-bench.sh with PROBE=1. A time near 40 ms means that the second write was held
back until the acknowledgement of the first (Nagle's algorithm with delayed acknowledgements).
"""

from __future__ import annotations

import os
import random
import select
import socket
import statistics
import struct
import subprocess
import sys
import time
from typing import IO

MIN_ARGS_WITH_GAP = 7  # the program, the mode, five arguments, and then the gap


def spin(micros: float) -> None:
    """Waits by looping, since a sleep is too coarse for microseconds."""
    end = time.perf_counter() + micros / 1e6
    while time.perf_counter() < end:
        pass


def server(port: int, gap_us: int, nodelay: bool, mode: str) -> None:
    """Answers every byte of a client, for ever."""
    listener = socket.socket()
    listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    listener.bind(("127.0.0.1", port))
    listener.listen(4)
    while True:
        conn, _ = listener.accept()
        if nodelay:
            conn.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        if mode.startswith("up-"):
            up_server(conn, mode)
            continue
        while conn.recv(1):
            if mode == "merge":
                conn.sendall(b"a" * 27 + b"b" * 41)
                continue
            conn.sendall(b"a" * (2000 if mode == "pad1" else 27))
            if mode == "reply":
                conn.recv(1)
            elif gap_us:
                spin(gap_us)
            conn.sendall(b"b" * (2000 if mode == "pad2" else 41))


def recv_exactly(conn: socket.socket, count: int) -> bytes:
    """Reads `count` bytes from a socket."""
    data = b""
    while len(data) < count:
        chunk = conn.recv(count - len(data))
        if not chunk:
            raise EOFError("closed")
        data += chunk
    return data


def up_server(conn: socket.socket, mode: str) -> None:
    """Times how long the second of two writes of the client takes to arrive."""
    first_size = 2000 if mode == "up-pad1" else 27
    second_size = 2000 if mode == "up-pad2" else 41
    if mode == "up-merge":
        first_size, second_size = 27, 41
    try:
        while True:
            recv_exactly(conn, first_size)
            first = time.perf_counter()
            if mode == "up-reply":
                conn.sendall(b"r")
            recv_exactly(conn, second_size)
            conn.sendall(struct.pack("d", (time.perf_counter() - first) * 1000))
    except EOFError:
        return


def read_exactly(fd: int, count: int, timeout: float = 10) -> bytes:
    """Reads `count` bytes from a file descriptor, or raises when none come in time."""
    data = b""
    end = time.monotonic() + timeout
    while len(data) < count:
        ready, _, _ = select.select([fd], [], [], max(0, end - time.monotonic()))
        if not ready:
            raise TimeoutError("no answer")
        chunk = os.read(fd, count - len(data))
        if not chunk:
            raise EOFError("closed")
        data += chunk
    return data


def spawn(host: str, port: int) -> tuple[subprocess.Popen[bytes], IO[bytes], int]:
    """Starts `ssh -W` to the probe server. Returns the process, its input, and its output."""
    child = subprocess.Popen(
        ["ssh", "-W", f"127.0.0.1:{port}", host], stdin=subprocess.PIPE, stdout=subprocess.PIPE
    )
    if child.stdin is None or child.stdout is None:
        raise RuntimeError("no pipes")
    return child, child.stdin, child.stdout.fileno()


def up_client(host: str, port: int, samples: int, mode: str, gap_us: int = 200) -> None:
    """Sends two writes and prints how long the second takes to arrive after the first."""
    _, stdin, out = spawn(host, port)
    first = b"a" * (2000 if mode == "up-pad1" else 27)
    second = b"b" * (2000 if mode == "up-pad2" else 41)
    gaps = []
    for _ in range(samples):
        if mode == "up-merge":
            stdin.write(first + second)
            stdin.flush()
        else:
            stdin.write(first)
            stdin.flush()
            if mode == "up-reply":
                read_exactly(out, 1)
            else:
                spin(gap_us)
            stdin.write(second)
            stdin.flush()
        gaps.append(struct.unpack("d", read_exactly(out, 8))[0])
        time.sleep(0.01 + random.random() * 0.12)
    stdin.close()
    gaps.sort()
    median = statistics.median(gaps)
    stalls = sum(1 for g in gaps if g > median + 10)
    print(
        f"second write arrives after the first: median {median:.2f} ms,"
        f" p99 {gaps[int(len(gaps) * 0.99)]:.2f},"
        f" max {gaps[-1]:.2f}, stalls {stalls} of {samples}"
    )


def client(host: str, port: int, samples: int, mode: str, gap_us: int = 200) -> None:
    """Reads two answers per byte and prints how long the second takes after the first."""
    if mode.startswith("up-"):
        up_client(host, port, samples, mode, gap_us)
        return
    first_size = 2000 if mode == "pad1" else 27
    second_size = 2000 if mode == "pad2" else 41
    if mode == "merge":
        first_size, second_size = 27, 41
    _, stdin, out = spawn(host, port)
    gaps = []
    for _ in range(samples):
        stdin.write(b"x")
        stdin.flush()
        read_exactly(out, first_size)
        first = time.perf_counter()
        if mode == "reply":
            stdin.write(b"y")
            stdin.flush()
        read_exactly(out, second_size)
        gaps.append((time.perf_counter() - first) * 1000)
        time.sleep(0.01 + random.random() * 0.12)
    stdin.close()
    gaps.sort()
    median = statistics.median(gaps)
    stalls = sum(1 for g in gaps if g > median + 10)
    print(
        f"second answer after the first: median {median:.2f} ms,"
        f" p99 {gaps[int(len(gaps) * 0.99)]:.2f},"
        f" max {gaps[-1]:.2f}, stalls {stalls} of {samples}"
    )


if __name__ == "__main__":
    mode = sys.argv[1]
    if mode == "server":
        server(int(sys.argv[2]), int(sys.argv[3]), sys.argv[4] == "1", sys.argv[5])
    else:
        gap = int(sys.argv[6]) if len(sys.argv) >= MIN_ARGS_WITH_GAP else 200
        client(sys.argv[2], int(sys.argv[3]), int(sys.argv[4]), sys.argv[5], gap)
