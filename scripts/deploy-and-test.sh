#!/usr/bin/env bash
# End-to-end test of the global-proxy (TUN) path:
#   deploy server -> start it -> run CLI client in Docker -> verify egress IP.
# Requires: target-linux/release/{entrotunnel-server,entrotunnel-cli} built.
set -euo pipefail
cd "$(dirname "$0")/.."

SERVER="${SERVER:-141.11.149.77}"
PORT="${PORT:-8443}"
SSH="ssh -o StrictHostKeyChecking=accept-new root@$SERVER"

echo "==> [1/5] deploy server binary"
$SSH 'mkdir -p /root/entrotunnel'
scp -q target-linux/release/entrotunnel-server "root@$SERVER:/root/entrotunnel/"

echo "==> [2/5] generate server config (capture psk + peer token)"
GEN=$($SSH 'cd /root/entrotunnel && ./entrotunnel-server gen-config -c server.toml')
echo "$GEN" | sed 's/^/    /'
PSK=$(echo "$GEN"   | sed -n 's/^noise_psk : //p')
PTOKEN=$(echo "$GEN" | sed -n 's/^example peer token: \(.*\) -> .*/\1/p')
[ -n "$PSK" ] && [ -n "$PTOKEN" ] || { echo "failed to parse psk/token"; exit 1; }

echo "==> [3/5] (re)start server"
# Kill by PID file (pkill -f would self-match this very ssh command line).
$SSH "cd /root/entrotunnel && \
      { [ -f server.pid ] && kill \$(cat server.pid) 2>/dev/null || true; }; sleep 1; \
      RUST_LOG=info nohup ./entrotunnel-server -c server.toml run > server.log 2>&1 & \
      echo \$! > server.pid; sleep 2; echo '--- server.log ---'; tail -n 15 server.log"

echo "==> [4/5] build client.toml"
cat > /tmp/et-client.toml <<EOF
name = "test"
server_host = "$SERVER"
server_port = $PORT
transport = "tcp"
token = "$PTOKEN"
noise_psk = "$PSK"
mode = "global_proxy"
client_name = "docker-cli"
tun_name = "et0"
http_listen = "127.0.0.1:7890"
tls_skip_verify = false
EOF

echo "==> [5/5] run client in Docker (TUN + NET_ADMIN), verify egress IP"
docker run --rm --platform linux/amd64 \
  --cap-add NET_ADMIN --device /dev/net/tun \
  -e SERVER="$SERVER" \
  -v "$PWD/target-linux/release/entrotunnel-cli:/usr/local/bin/entrotunnel-cli:ro" \
  -v /tmp/et-client.toml:/etc/client.toml:ro \
  debian:bullseye-slim bash -c '
    set -e
    apt-get update -qq >/dev/null && apt-get install -y -qq iproute2 curl ca-certificates >/dev/null
    BEFORE=$(curl -s --max-time 10 https://api.ipify.org || echo FAIL)
    echo "    egress BEFORE tunnel : $BEFORE"
    RUST_LOG=info entrotunnel-cli -c /etc/client.toml run > /tmp/cli.log 2>&1 &
    sleep 5
    echo "    --- client log ---"; sed "s/^/    /" /tmp/cli.log
    echo "    --- et0 ---"; ip -br addr show et0 2>/dev/null | sed "s/^/    /" || echo "    (no et0)"
    AFTER=$(curl -s --max-time 15 https://api.ipify.org || echo FAIL)
    echo "    egress THROUGH tunnel: $AFTER"
    if [ "$AFTER" = "$SERVER" ]; then echo "    RESULT: PASS ✅ (egress == server IP)";
    else echo "    RESULT: FAIL ❌ (expected $SERVER, got $AFTER)"; fi
  '

echo "==> stopping remote server"
$SSH 'cd /root/entrotunnel && [ -f server.pid ] && kill $(cat server.pid) 2>/dev/null || true'
