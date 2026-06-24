#!/usr/bin/env bash
# Multi-server test: one client config lists two servers (same box, different
# transports/ports). We select each with `--server <name>` and confirm the
# client connects over THAT server (right transport/port) and egresses via it.
set -euo pipefail
cd "$(dirname "$0")/.."

SERVER="${SERVER:-141.11.149.77}"
SSH="ssh -o StrictHostKeyChecking=accept-new root@$SERVER"
CLI="$PWD/target-linux/release/entrotunnel-cli"

docker image inspect et-test >/dev/null 2>&1 || docker build --platform linux/amd64 -t et-test - >/dev/null <<'DOCKER'
FROM debian:bullseye-slim
RUN apt-get update -qq && apt-get install -y -qq iproute2 curl ca-certificates && rm -rf /var/lib/apt/lists/*
DOCKER

echo "==> deploy server with tcp + ws listeners"
$SSH 'mkdir -p /root/entrotunnel'
scp -q target-linux/release/entrotunnel-server "root@$SERVER:/root/entrotunnel/"
PSK=$($SSH 'cd /root/entrotunnel && ./entrotunnel-server gen-config -c /tmp/seed.toml' | sed -n 's/^noise_psk : //p')
TOKEN="multiserver-token-0002"
$SSH "cat > /root/entrotunnel/server.toml" <<EOF
[[listeners]]
transport = "tcp"
bind = "0.0.0.0:8443"
[[listeners]]
transport = "ws"
bind = "0.0.0.0:8444"

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
admin_token = "ms"

[[peers]]
name = "p"
token = "$TOKEN"
ip = "10.66.0.2"
EOF
$SSH "cd /root/entrotunnel && { [ -f server.pid ] && kill \$(cat server.pid) 2>/dev/null || true; }; \
      sleep 1; RUST_LOG=info nohup ./entrotunnel-server -c server.toml run > server.log 2>&1 & \
      echo \$! > server.pid; sleep 2; tail -n 6 server.log | grep listening"

# One client config, two servers (tcp:8443 and ws:8444), default selects via-tcp.
cat > /tmp/et-ms-client.toml <<EOF
name = "ms"
selected_server = "via-tcp"
mode = "global_proxy"
tun_name = "et0"
http_listen = "127.0.0.1:7890"

[[servers]]
name = "via-tcp"
host = "$SERVER"
port = 8443
transport = "tcp"
token = "$TOKEN"
noise_psk = "$PSK"

[[servers]]
name = "via-ws"
host = "$SERVER"
port = 8444
transport = "ws"
token = "$TOKEN"
noise_psk = "$PSK"
tls_skip_verify = true
EOF

echo "==> 'servers' listing (default selection = via-tcp)"
docker run --rm --platform linux/amd64 \
  -v "$CLI:/usr/local/bin/entrotunnel-cli:ro" \
  -v /tmp/et-ms-client.toml:/etc/client.toml:ro \
  et-test entrotunnel-cli -c /etc/client.toml servers | sed 's/^/    /'

run_via() { # server-name expected-transport
  local NAME=$1 XPORT=$2
  echo ""
  echo "================ --server $NAME (expect $XPORT) ================"
  docker run --rm --platform linux/amd64 \
    --cap-add NET_ADMIN --device /dev/net/tun \
    -e SERVER="$SERVER" -e NAME="$NAME" -e XPORT="$XPORT" \
    -v "$CLI:/usr/local/bin/entrotunnel-cli:ro" \
    -v /tmp/et-ms-client.toml:/etc/client.toml:ro \
    et-test bash -c '
      RUST_LOG=info entrotunnel-cli -c /etc/client.toml --server "$NAME" run > /tmp/cli.log 2>&1 &
      sleep 5
      echo "    connect log: $(grep -o "connecting.*" /tmp/cli.log | head -1)"
      A=$(curl -s --max-time 15 https://api.ipify.org || echo FAIL)
      echo "    egress: $A"
      if grep -q "$XPORT" /tmp/cli.log && [ "$A" = "$SERVER" ]; then
        echo "    RESULT($NAME): PASS ✅ (used $XPORT, egress=server)"
      else
        echo "    RESULT($NAME): FAIL ❌"; sed "s/^/      /" /tmp/cli.log
      fi
    '
}

run_via via-tcp "8443"
run_via via-ws  "8444"

echo ""
echo "==> stop server"
$SSH 'cd /root/entrotunnel && [ -f server.pid ] && kill $(cat server.pid) 2>/dev/null || true'
