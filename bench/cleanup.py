#!/usr/bin/env python3
# ruff: noqa: T201
"""Checks which ways of leaving a session leave a server process behind.

    bench/cleanup.py --host localhost --ssj target/release/ssj --server target/release/ssj-server

Each case starts a session on a pseudo terminal, ends it one way, and counts the server daemons
that are still there after a few seconds. The expectation is printed next to the result, and the
exit code is 1 if any case differs.
"""

from __future__ import annotations

import argparse
import contextlib
import os
import signal
import subprocess
import sys
import time
from collections.abc import Callable
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from bench import Terminal

Action = Callable[[Terminal], None]


def daemons() -> int:
    """Counts the server daemons that run."""
    out = subprocess.run(
        ["pgrep", "-f", "daemon-child"], capture_output=True, text=True, check=False
    ).stdout
    return len(out.split())


def wait_gone(seconds: float) -> bool:
    """Waits until no server daemon is left, for at most `seconds`."""
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        if daemons() == 0:
            return True
        time.sleep(0.2)
    return daemons() == 0


def reap(terminal: Terminal) -> None:
    """Collects the client process, if it is not collected yet."""
    with contextlib.suppress(ChildProcessError):
        os.waitpid(terminal.pid, 0)


class Args(argparse.Namespace):
    """The command line, with the type of every option."""

    host: str
    ssj: str
    server: str
    wait: float


def main() -> None:
    """Runs every case and prints the result."""
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--host", default="localhost")
    parser.add_argument("--ssj", default="target/release/ssj")
    parser.add_argument("--server", default="target/release/ssj-server")
    parser.add_argument(
        "--wait", type=float, default=6, help="seconds to wait for the server to end"
    )
    args = parser.parse_args(namespace=Args())
    argv = [
        str(Path(args.ssj).resolve()),
        "--server-path",
        str(Path(args.server).resolve()),
        args.host,
    ]

    def exit_command(t: Terminal) -> None:
        t.send("exit\n")

    def ctrl_d(t: Terminal) -> None:
        t.send("\x04")

    def signal_client(number: int) -> Action:
        def action(t: Terminal) -> None:
            os.kill(t.pid, number)

        return action

    def detach(t: Terminal) -> None:
        t.send("\n~.")

    # name, how the session ends, whether the server should be gone afterwards
    cases: list[tuple[str, Action, bool]] = [
        ("exit", exit_command, True),
        ("Ctrl+D", ctrl_d, True),
        ("terminal closed (SIGHUP)", signal_client(signal.SIGHUP), True),
        ("ssj killed (SIGTERM)", signal_client(signal.SIGTERM), True),
        ("detach (~.)", detach, False),
        ("ssj killed hard (SIGKILL)", signal_client(signal.SIGKILL), False),
    ]
    failed = False
    for name, action, gone in cases:
        subprocess.run(["pkill", "-f", "daemon-child"], check=False)
        wait_gone(3)
        terminal = Terminal(argv)
        terminal.send("echo READY$((6*7))\n")
        terminal.wait_for("READY42", 60)
        action(terminal)
        time.sleep(0.3)
        with contextlib.suppress(OSError):
            for _ in range(20):
                terminal.read_some()
                time.sleep(0.05)
        left = not wait_gone(args.wait)
        ok = left != gone
        failed |= not ok
        print(
            f"{name:28} server {'left behind' if left else 'gone':12}"
            f" expected {'gone' if gone else 'kept':5} {'ok' if ok else 'DIFFERENT'}"
        )
        with contextlib.suppress(ProcessLookupError):
            os.kill(terminal.pid, signal.SIGKILL)
        reap(terminal)
    subprocess.run(["pkill", "-f", "daemon-child"], check=False)
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
