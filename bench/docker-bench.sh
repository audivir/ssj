#!/bin/sh
# Runs bench/bench.py between two containers, a client and a server, on one docker network.
#
# The link is simulated on the network interfaces of the two containers, so only the traffic
# between them is delayed or dropped. On the loopback (bench/linux-bench.sh) the TCP transport
# crosses the simulated link twice, since the hop from sshd to the server port is on the
# loopback too, and its numbers are not those of a real host.
#
#   bench/docker-bench.sh [bench.py arguments]
#
#   NETEM="delay 40ms loss 1%"    the link; empty for none
#   IMAGE=ubuntu:24.04            the system of both containers (the version of sshd matters)
#   BUILD=1                       build the static musl binaries first (default), 0 to reuse
#   CAPTURE=1                     record the hop from sshd to the server port on the server's
#                                 loopback and print its last packets (for TCP stalls)
#
# Examples: bench/docker-bench.sh --raw-bulk --bulk-mb 64 --samples 100
#           NETEM="delay 20ms" bench/docker-bench.sh --only ssh,udp,tcp --json
set -e
cd "$(dirname "$0")/.."
IMAGE=${IMAGE:-ubuntu:24.04}
NETEM=${NETEM-delay 40ms loss 1%}
BUILD=${BUILD:-1}
CAPTURE=${CAPTURE:-0}
name=ssjbench-$$
# inside the checkout, since docker on a Mac or under colima only shares some directories
mkdir -p target
keys=$(mktemp -d "$PWD/target/ssjbench-keys.XXXXXX")

cleanup() {
    docker rm -f "$name-server" >/dev/null 2>&1 || true
    docker network rm "$name" >/dev/null 2>&1 || true
    rm -rf "$keys"
}
trap cleanup EXIT INT TERM

if [ "$BUILD" = 1 ]; then
    # static binaries run on any image, whatever its libc
    docker run --rm -v "$PWD":/src:ro -v ssj-target-musl:/target -e CARGO_TARGET_DIR=/target \
        -w /src rust:alpine sh -c \
        'apk add --no-cache musl-dev gcc >/dev/null && cargo build --release -p ssj-client -p ssj-server 2>&1 | tail -1'
fi

ssh-keygen -q -t ed25519 -N "" -f "$keys/id"
docker network create "$name" >/dev/null

# the simulated link on the egress of an interface; offloads are off, so that the loss hits
# packets of the size of a real link and not segments of 64 KiB
link='
if [ -n "$NETEM" ]; then
    ethtool -K eth0 tso off gso off gro off >/dev/null 2>&1 || true
    tc qdisc add dev eth0 root netem $NETEM
fi'
install='apt-get update -qq >/dev/null 2>&1 && apt-get install -y -qq iproute2 ethtool openssh-server openssh-client python3 tcpdump >/dev/null 2>&1'
capture=
[ "$CAPTURE" = 1 ] && capture='tcpdump -i lo -n -tt -s 128 -w /cap.pcap "tcp and not port 22" >/dev/null 2>&1 &'

docker run -d --name "$name-server" --network "$name" --hostname server --cap-add NET_ADMIN \
    -e NETEM="$NETEM" -v ssj-target-musl:/target:ro -v "$keys/id.pub":/id.pub:ro "$IMAGE" sh -c "
$install
mkdir -p /run/sshd /root/.ssh
ssh-keygen -A >/dev/null
cat /id.pub > /root/.ssh/authorized_keys
chmod 700 /root/.ssh && chmod 600 /root/.ssh/authorized_keys
sed -i '/^#*AllowTcpForwarding/d' /etc/ssh/sshd_config
echo 'AllowTcpForwarding yes' >> /etc/ssh/sshd_config
$link
$capture
exec /usr/sbin/sshd -D -e" >/dev/null

docker run --rm --network "$name" --cap-add NET_ADMIN -e NETEM="$NETEM" \
    -v "$PWD":/src:ro -v ssj-target-musl:/target:ro -v "$keys/id":/id:ro "$IMAGE" sh -c "
$install
mkdir -p /root/.ssh
cp /id /root/.ssh/id_ed25519 && chmod 600 /root/.ssh/id_ed25519
printf 'Host server\n  StrictHostKeyChecking no\n  UserKnownHostsFile /dev/null\n  LogLevel ERROR\n' > /root/.ssh/config
$link
n=0
until ssh -o BatchMode=yes -o ConnectTimeout=3 root@server true 2>/dev/null; do
    n=\$((n + 1)); [ \$n -gt 60 ] && { echo 'the server container does not answer' >&2; exit 1; }
    sleep 1
done
echo \"link: \${NETEM:-none}; \$(ssh -V 2>&1)\"
cd /src && python3 bench/bench.py --host root@server --ssj /target/release/ssj \
    --server /target/release/ssj-server $*"
status=$?
if [ "$CAPTURE" = 1 ]; then
    docker exec "$name-server" sh -c 'kill -INT $(pidof tcpdump); sleep 1; tcpdump -r /cap.pcap -nn -tt -q 2>/dev/null | grep -v " tcp 0" | tail -80'
fi
exit $status
