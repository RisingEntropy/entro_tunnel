#!/usr/bin/env bash
# Test global-proxy egress over ALL THREE transports (tcp+Noise / wss / quic).
# Server runs three listeners; the client connects over each in turn and we
# assert the egress IP becomes the server's IP. Needs a full-feature build.
set -euo pipefail
cd "$(dirname "$0")/.."

SERVER="${SERVER:-141.11.149.77}"
SSH="ssh -o StrictHostKeyChecking=accept-new root@$SERVER"
CLI="$PWD/target-linux/release/entrotunnel-cli"
TOKEN="multi-transport-token-0002"

docker image inspect et-test >/dev/null 2>&1 || docker build --platform linux/amd64 -t et-test - >/dev/null <<'DOCKER'
FROM debian:bullseye-slim
RUN apt-get update -qq && apt-get install -y -qq iproute2 iputils-ping curl ca-certificates \
    && rm -rf /var/lib/apt/lists/*
DOCKER

echo "==> deploy server + 3-listener config (server self-signs a TLS cert)"
$SSH 'mkdir -p /root/entrotunnel'
scp -q target-linux/release/entrotunnel-server "root@$SERVER:/root/entrotunnel/"
PSK=$($SSH 'cd /root/entrotunnel && ./entrotunnel-server gen-config -c /tmp/seed.toml' | sed -n 's/^noise_psk : //p')
$SSH "cat > /root/entrotunnel/server.toml" <<EOF
[[listeners]]
transport = "tcp"
bind = "0.0.0.0:8443"
[[listeners]]
transport = "ws"
bind = "0.0.0.0:8444"
[[listeners]]
transport = "quic"
bind = "0.0.0.0:8445"

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
admin_token = "multitest"

[[peers]]
name = "p"
token = "$TOKEN"
ip = "10.66.0.2"
EOF

$SSH "cd /root/entrotunnel && { [ -f server.pid ] && kill \$(cat server.pid) 2>/dev/null || true; }; \
      sleep 1; RUST_LOG=info nohup ./entrotunnel-server -c server.toml run > server.log 2>&1 & \
      echo \$! > server.pid; sleep 2; echo '--- server.log ---'; tail -n 12 server.log"

test_transport() { # transport port
  local T=$1 P=$2
  echo ""
  echo "================ transport=$T  port=$P ================"
  cat > "/tmp/et-$T.toml" <<EOF
name = "$T"
server_host = "$SERVER"
server_port = $P
transport = "$T"
token = "$TOKEN"
noise_psk = "$PSK"
mode = "global_proxy"
client_name = "$T-client"
tun_name = "et0"
http_listen = "127.0.0.1:7890"
tls_skip_verify = true
EOF
  docker run --rm --platform linux/amd64 \
    --cap-add NET_ADMIN --device /dev/net/tun \
    -e SERVER="$SERVER" -e T="$T" \
    -v "$CLI:/usr/local/bin/entrotunnel-cli:ro" \
    -v "/tmp/et-$T.toml:/etc/client.toml:ro" \
    et-test bash -c '
      RUST_LOG=info entrotunnel-cli -c /etc/client.toml run > /tmp/cli.log 2>&1 &
      sleep 5
      A=$(curl -s --max-time 15 https://api.ipify.org || echo FAIL)
      echo "    egress THROUGH $T tunnel: $A"
      if [ "$A" = "$SERVER" ]; then echo "    RESULT($T): PASS ✅";
      else echo "    RESULT($T): FAIL ❌"; echo "    --- client log ---"; sed "s/^/      /" /tmp/cli.log; fi
    '
}

test_transport tcp 8443
test_transport ws 8444
test_transport quic 8445

echo ""
echo "==> stop server"
$SSH 'cd /root/entrotunnel && [ -f server.pid ] && kill $(cat server.pid) 2>/dev/null || true'
