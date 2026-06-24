#!/usr/bin/env bash
# Test HTTP-proxy mode: a local proxy listener tunnels only proxied traffic
# through the server. No TUN / no privileges needed. We verify by curling
# through the proxy and checking the egress IP is the server's.
set -euo pipefail
cd "$(dirname "$0")/.."

SERVER="${SERVER:-141.11.149.77}"
PORT=8443
SSH="ssh -o StrictHostKeyChecking=accept-new root@$SERVER"
CLI="$PWD/target-linux/release/entrotunnel-cli"

docker image inspect et-test >/dev/null 2>&1 || docker build --platform linux/amd64 -t et-test - >/dev/null <<'DOCKER'
FROM debian:bullseye-slim
RUN apt-get update -qq && apt-get install -y -qq curl ca-certificates && rm -rf /var/lib/apt/lists/*
DOCKER

echo "==> deploy + start server (tcp listener)"
$SSH 'mkdir -p /root/entrotunnel'
scp -q target-linux/release/entrotunnel-server "root@$SERVER:/root/entrotunnel/"
GEN=$($SSH 'cd /root/entrotunnel && ./entrotunnel-server gen-config -c server.toml')
PSK=$(echo "$GEN"   | sed -n 's/^noise_psk : //p')
PTOKEN=$(echo "$GEN" | sed -n 's/^example peer token: \(.*\) -> .*/\1/p')
$SSH "cd /root/entrotunnel && { [ -f server.pid ] && kill \$(cat server.pid) 2>/dev/null || true; }; \
      sleep 1; RUST_LOG=info nohup ./entrotunnel-server -c server.toml run > server.log 2>&1 & \
      echo \$! > server.pid; sleep 2; tail -n 5 server.log"

cat > /tmp/et-http.toml <<EOF
name = "http"
server_host = "$SERVER"
server_port = $PORT
transport = "tcp"
token = "$PTOKEN"
noise_psk = "$PSK"
mode = "http_proxy"
client_name = "http-client"
tun_name = "et0"
http_listen = "127.0.0.1:7890"
tls_skip_verify = false
EOF

echo "==> run client (HTTP-proxy mode, no privileges) + curl through it"
docker run --rm --platform linux/amd64 \
  -e SERVER="$SERVER" \
  -v "$CLI:/usr/local/bin/entrotunnel-cli:ro" \
  -v /tmp/et-http.toml:/etc/client.toml:ro \
  et-test bash -c '
    DIRECT=$(curl -s --max-time 10 https://api.ipify.org || echo FAIL)
    echo "    egress DIRECT (no proxy)   : $DIRECT"
    RUST_LOG=info entrotunnel-cli -c /etc/client.toml run > /tmp/cli.log 2>&1 &
    sleep 3
    VIA=$(curl -s --max-time 15 -x http://127.0.0.1:7890 https://api.ipify.org || echo FAIL)
    echo "    egress VIA proxy (CONNECT) : $VIA"
    HTTP=$(curl -s --max-time 15 -x http://127.0.0.1:7890 http://api.ipify.org || echo FAIL)
    echo "    egress VIA proxy (plain)   : $HTTP"
    if [ "$VIA" = "$SERVER" ]; then echo "    RESULT: PASS ✅ (proxied HTTPS egresses via server)";
    else echo "    RESULT: FAIL ❌ (expected $SERVER, got $VIA)"; echo "    --- client log ---"; sed "s/^/      /" /tmp/cli.log; fi
  '

echo "==> stop server"
$SSH 'cd /root/entrotunnel && [ -f server.pid ] && kill $(cat server.pid) 2>/dev/null || true'
