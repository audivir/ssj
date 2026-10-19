#!/usr/bin/env python3
"""Checks which ways of leaving a session leave a server process behind.

    bench/cleanup.py --host localhost --ssj target/release/ssj --server target/release/ssj-server

Each case starts a session on a pseudo terminal, ends it one way, and counts the server daemons
that are still there after a few seconds. The expectation is printed next to the result, and the
exit code is 1 if any case differs.
"""

import argparse
import os
import signal
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from bench import Terminal  # noqa: E402


def daemons():
    out = subprocess.run(["pgrep", "-f", "daemon-child"], capture_output=True, text=True).stdout
    return len(out.split())


def wait_gone(seconds):
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        if daemons() == 0:
            return True
        time.sleep(0.2)
    return daemons() == 0


def reap(terminal):
    try:
        os.waitpid(terminal.pid, 0)
    except ChildProcessError:
        pass


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--host", default="localhost")
    parser.add_argument("--ssj", default="target/release/ssj")
    parser.add_argument("--server", default="target/release/ssj-server")
    parser.add_argument("--wait", type=float, default=6, help="seconds to wait for the server to end")
    args = parser.parse_args()
    argv = [os.path.abspath(args.ssj), "--server-path", os.path.abspath(args.server), args.host]

    def exit_command(t):
        t.send("exit\n")

    def ctrl_d(t):
        t.send("\x04")

    def signal_client(number):
        def action(t):
            os.kill(t.pid, number)

        return action

    def detach(t):
        t.send("\n~.")

    # name, how the session ends, whether the server should be gone afterwards
    cases = [
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
        try:
            for _ in range(20):
                terminal.read_some()
                time.sleep(0.05)
        except OSError:
            pass
        left = not wait_gone(args.wait)
        ok = left != gone
        failed |= not ok
        print(f"{name:28} server {'left behind' if left else 'gone':12} expected {'gone' if gone else 'kept':5} {'ok' if ok else 'DIFFERENT'}")
        try:
            os.kill(terminal.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        reap(terminal)
    subprocess.run(["pkill", "-f", "daemon-child"], check=False)
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
