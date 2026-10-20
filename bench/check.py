#!/usr/bin/env python3
"""Checks the results of bench/docker-bench.sh against limits, and prints them as a table.

    bench/check.py results/clean.json results/lossy.json

The limits are wide, since a machine in the cloud is noisy: they catch a change that breaks
something and not one that costs a few percent.

- on the clean link: ssj over UDP answers a keystroke within 5 ms (median), over TCP and the
  ssh channel within 10 ms, and at most 1 percent of the echoes stall (see SPEC.md, "Small
  packets on the TCP transport");
- on the lossy link: ssj over UDP moves bulk output at least 3 times as fast as ssh does.

Exit code 1 if a limit is broken.
"""

import json
import sys


def load(path):
    text = open(path).read()
    # the script prints a line or two before the JSON
    return json.loads(text[text.index("{") :])


def table(title, results):
    lines = [f"### {title}", "", "| client | bulk MB/s | median ms | p95 ms | max ms | stalls |", "| --- | ---: | ---: | ---: | ---: | ---: |"]
    for name, r in results.items():
        lat = r["latency"]
        lines.append(
            f"| {name} | {r['bulk']['megabytes_per_second']} | {lat['median_ms']} | {lat['p95_ms']}"
            f" | {lat['max_ms']} | {lat.get('stalls', '-')} |"
        )
    return "\n".join(lines) + "\n"


def main():
    clean_path, lossy_path = sys.argv[1], sys.argv[2]
    clean, lossy = load(clean_path), load(lossy_path)
    print(table("Clean link", clean))
    print(table("Lossy link", lossy))
    broken = []
    for name, limit in (("udp", 5), ("tcp", 10), ("stdio", 10)):
        lat = clean[name]["latency"]
        if lat["median_ms"] > limit:
            broken.append(f"clean link: {name} keystroke median {lat['median_ms']} ms is above {limit} ms")
        allowed = max(1, int(lat["samples"] * 0.01))
        if lat["stalls"] > allowed:
            broken.append(f"clean link: {name} had {lat['stalls']} stalls in {lat['samples']} keystrokes (limit {allowed})")
    ssh_bulk = lossy["ssh"]["bulk"]["megabytes_per_second"]
    udp_bulk = lossy["udp"]["bulk"]["megabytes_per_second"]
    if udp_bulk < 3 * ssh_bulk:
        broken.append(f"lossy link: ssj over UDP moves {udp_bulk} MB/s, ssh {ssh_bulk} MB/s (UDP should be 3 times as fast)")
    for line in broken:
        print(f"BROKEN: {line}")
    sys.exit(1 if broken else 0)


if __name__ == "__main__":
    main()
