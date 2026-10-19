#!/bin/sh
# Runs bench/bench.py inside a Linux container with its own sshd, in four situations: the
# loopback with zeros and with text, a slow link, and a lossy link. The image is rust:latest
# (glibc) or rust:alpine (musl); the container needs NET_ADMIN for the two link situations:
#   docker run --rm --cap-add NET_ADMIN -v "$PWD":/src:ro -v ssj-target:/target \
#     -e CARGO_TARGET_DIR=/target -w /src rust:latest sh bench/linux-bench.sh
set -e
if command -v apk >/dev/null 2>&1; then
    apk add --no-cache musl-dev gcc python3 openssh iproute2 >/dev/null 2>&1
    # sshd refuses a locked account, and root has no password on this image
    sed -i 's/^root:!/root:*/' /etc/shadow
else
    apt-get update -qq >/dev/null 2>&1
    apt-get install -y -qq python3 openssh-server openssh-client iproute2 >/dev/null 2>&1
fi
mkdir -p /run/sshd
ssh-keygen -A >/dev/null
# The TCP transport goes through `ssh -W`, which some images forbid by default. sshd uses the
# first value it finds, so the line goes first and the image's own line is removed.
sed -i '/^#*AllowTcpForwarding/d' /etc/ssh/sshd_config
{ echo "AllowTcpForwarding yes"; cat /etc/ssh/sshd_config; } > /tmp/sshd_config.new
cat /tmp/sshd_config.new > /etc/ssh/sshd_config
/usr/sbin/sshd
mkdir -p ~/.ssh
ssh-keygen -q -t ed25519 -N "" -f ~/.ssh/id_ed25519
cat ~/.ssh/id_ed25519.pub >> ~/.ssh/authorized_keys
chmod 600 ~/.ssh/authorized_keys
ssh-keyscan localhost >> ~/.ssh/known_hosts 2>/dev/null
# STARTUP_RC_DELAY=0.4 makes the login shell slow to start, as a heavy shell configuration does
[ -n "$STARTUP_RC_DELAY" ] && echo "sleep $STARTUP_RC_DELAY" >> ~/.bash_profile
ssh -o BatchMode=yes localhost 'echo ssh-ok; uname -m'
cargo build --release -p ssj-client -p ssj-server 2>&1 | tail -1
bench="python3 bench/bench.py --host localhost --ssj /target/release/ssj --server /target/release/ssj-server"
# CONGESTION=cubic or bbr, and ONLY=ssh,udp, narrow a run to one controller and some clients
[ -n "$CONGESTION" ] && bench="$bench --congestion $CONGESTION"
[ -n "$ONLY" ] && bench="$bench --only $ONLY"

# SCENARIOS picks some of them (zeros text slow lossy clean fast start cleanup), for example SCENARIOS="slow lossy"
scenarios=${SCENARIOS:-"zeros text slow lossy"}
want() { case " $scenarios " in *" $1 "*) return 0 ;; *) return 1 ;; esac; }

if want cleanup; then
    echo "== which ways of leaving a session leave the server behind"
    python3 bench/cleanup.py --host localhost --ssj /target/release/ssj --server /target/release/ssj-server
fi

if want zeros; then
    echo "== loopback, zeros (the transport itself)"
    $bench --raw-bulk --bulk-mb "${BULK_MB:-256}" --samples 150 --rounds "${ROUNDS:-3}"
fi

if want text; then
    echo "== loopback, log-like text"
    $bench --text-bulk --bulk-mb 64 --paste-mb 4 --samples 20 --rounds 2
fi

if (want slow || want lossy || want clean || want fast || want start) && command -v tc >/dev/null 2>&1 && tc qdisc add dev lo root netem delay 1ms 2>/dev/null; then
    tc qdisc del dev lo root
    # The loopback has an MTU of 64 KiB, so TCP would send segments that are 40 times larger than
    # a QUIC packet, and a loss rate would hit them far less often. A real link has about 1500.
    ip link set dev lo mtu 1500
    if want slow; then
        echo "== slow link: 8 Mbit/s, 25 ms each way, log-like text"
        $bench --text-bulk --bulk-mb 8 --paste-mb 2 --samples 50 --rounds 1 --netem "rate 8mbit delay 25ms"
    fi
    if want clean; then
        echo "== clean link: 100 Mbit/s, 10 ms each way, zeros"
        $bench --raw-bulk --bulk-mb 32 --paste-mb 4 --samples 50 --rounds 1 --netem "rate 100mbit delay 10ms"
    fi
    if want fast; then
        echo "== fast link: 1 Gbit/s, 1 ms each way, zeros"
        $bench --raw-bulk --bulk-mb 128 --paste-mb 4 --samples 50 --rounds 1 --netem "rate 1gbit delay 1ms"
    fi
    if want start; then
        echo "== start and end of a session, 50 ms each way"
        $bench --startup 5 --netem "delay 50ms"
    fi
    if want lossy; then
        echo "== lossy link: 1 % loss, 40 ms each way, zeros"
        $bench --raw-bulk --bulk-mb 32 --paste-mb 4 --samples 50 --rounds 1 --netem "delay 40ms loss 1%"
    fi
    ip link set dev lo mtu 65536
elif want slow || want lossy || want clean || want fast || want start; then
    echo "== no tc or no NET_ADMIN: the slow and the lossy link are skipped"
fi
