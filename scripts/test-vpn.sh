#!/usr/bin/env bash
# Two-client VPN test: peers A (10.66.0.2) and B (10.66.0.3) on the same server
# must reach each other by virtual IP. Verified by A pinging B over the tunnel.
set -euo pipefail
cd "$(dirname "$0")/.."

SERVER="${SERVER:-141.11.149.77}"
PORT=8443
SSH="ssh -o StrictHostKeyChecking=accept-new root@$SERVER"
TOKEN_A="vpn-peer-a-token-0002"
TOKEN_B="vpn-peer-b-token-0003"
CLI="$PWD/target-linux/release/entrotunnel-cli"

echo "==> prebuild small test image (iproute2 + ping)"
docker build --platform linux/amd64 -t et-test - >/dev/null <<'DOCKER'
FROM debian:bullseye-slim
RUN apt-get update -qq && apt-get install -y -qq iproute2 iputils-ping ca-certificates \
    && rm -rf /var/lib/apt/lists/*
DOCKER

echo "==> deploy server + write two-peer config"
$SSH 'mkdir -p /root/entrotunnel'
scp -q target-linux/release/entrotunnel-server "root@$SERVER:/root/entrotunnel/"
PSK=$($SSH 'cd /root/entrotunnel && ./entrotunnel-server gen-config -c /tmp/seed.toml' | sed -n 's/^noise_psk : //p')
$SSH "cat > /root/entrotunnel/server.toml" <<EOF
[[listeners]]
transport = "tcp"
bind = "0.0.0.0:$PORT"

[network]
subnet = "10.66.0.0/24"
gateway = "10.66.0.1"
mtu = 1380
dns = ["8.8.8.8", "1.1.1.1"]
tun_name = "et0"

[security]
noise_psk = "$PSK"

[web]
bind = "127.0.0.1:9000"
admin_token = "vpntest"

[[peers]]
name = "peerA"
token = "$TOKEN_A"
ip = "10.66.0.2"

[[peers]]
name = "peerB"
token = "$TOKEN_B"
ip = "10.66.0.3"
EOF

echo "==> (re)start server"
$SSH "cd /root/entrotunnel && { [ -f server.pid ] && kill \$(cat server.pid) 2>/dev/null || true; }; \
      sleep 1; RUST_LOG=info nohup ./entrotunnel-server -c server.toml run > server.log 2>&1 & \
      echo \$! > server.pid; sleep 2; tail -n 6 server.log"

mk_conf() {
cat <<EOF
name = "$1"
server_host = "$SERVER"
server_port = $PORT
transport = "tcp"
token = "$2"
noise_psk = "$PSK"
mode = "vpn"
client_name = "$1"
tun_name = "et0"
http_listen = "127.0.0.1:7890"
tls_skip_verify = false
EOF
}
mk_conf peerA "$TOKEN_A" > /tmp/et-vpn-a.toml
mk_conf peerB "$TOKEN_B" > /tmp/et-vpn-b.toml

echo "==> start peer B (detached, holds 10.66.0.3)"
docker rm -f et-vpn-b >/dev/null 2>&1 || true
docker run -d --name et-vpn-b --platform linux/amd64 \
  --cap-add NET_ADMIN --device /dev/net/tun \
  -v "$CLI:/usr/local/bin/entrotunnel-cli:ro" \
  -v /tmp/et-vpn-b.toml:/etc/client.toml:ro \
  et-test entrotunnel-cli -c /etc/client.toml run >/dev/null
sleep 5
echo "    peerB log:"; docker logs et-vpn-b 2>&1 | sed 's/^/      /' | tail -n 4

echo "==> peer A: connect + ping peer B over the VPN"
docker run --rm --platform linux/amd64 \
  --cap-add NET_ADMIN --device /dev/net/tun \
  -v "$CLI:/usr/local/bin/entrotunnel-cli:ro" \
  -v /tmp/et-vpn-a.toml:/etc/client.toml:ro \
  et-test bash -c '
    RUST_LOG=info entrotunnel-cli -c /etc/client.toml run > /tmp/cli.log 2>&1 &
    sleep 4
    echo "    peerA et0: $(ip -br addr show et0 | tr -s " ")"
    echo "    --- ping 10.66.0.3 ---"
    if ping -c 3 -W 3 10.66.0.3 | sed "s/^/    /"; then
      echo "    RESULT: PASS ✅ (peers reach each other by virtual IP)"
    else
      echo "    RESULT: FAIL ❌"; echo "    client log:"; sed "s/^/      /" /tmp/cli.log
    fi
  '

echo "==> cleanup"
docker rm -f et-vpn-b >/dev/null 2>&1 || true
$SSH 'cd /root/entrotunnel && [ -f server.pid ] && kill $(cat server.pid) 2>/dev/null || true'
