#!/bin/sh
# Runs bench/bench.py inside a Linux container with its own sshd, for example:
#   docker run --rm -v "$PWD":/src:ro -v ssj-target:/target -e CARGO_TARGET_DIR=/target -w /src rust:latest sh bench/linux-bench.sh
set -e
apt-get update -qq >/dev/null 2>&1
apt-get install -y -qq python3 openssh-server openssh-client >/dev/null 2>&1
mkdir -p /run/sshd; ssh-keygen -A >/dev/null; /usr/sbin/sshd
mkdir -p ~/.ssh; ssh-keygen -q -t ed25519 -N "" -f ~/.ssh/id_ed25519; cat ~/.ssh/id_ed25519.pub >> ~/.ssh/authorized_keys; chmod 600 ~/.ssh/authorized_keys
ssh-keyscan localhost >> ~/.ssh/known_hosts 2>/dev/null
ssh -o BatchMode=yes localhost 'echo ssh-ok; uname -m'
cargo build --release -p ssj-client -p ssj-server 2>&1 | tail -1
python3 bench/bench.py --host localhost --ssj /target/release/ssj --server /target/release/ssj-server --raw-bulk --bulk-mb 256 --samples 150 --rounds 3
