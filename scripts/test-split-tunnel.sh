#!/usr/bin/env bash
# Split-tunnel routing test (global-proxy mode):
#   rule "1.1.1.1/32 -> direct" must bypass the tunnel (egress = container IP),
#   while the un-ruled 1.0.0.1 still goes through the tunnel (egress = server IP).
# Both checked via Cloudflare's https://<ip>/cdn-cgi/trace which echoes the
# client's public IP. A domain rule (example.com -> direct) shows DNS resolution.
set -euo pipefail
cd "$(dirname "$0")/.."

SERVER="${SERVER:-141.11.149.77}"
PORT=8443
SSH="ssh -o StrictHostKeyChecking=accept-new root@$SERVER"
CLI="$PWD/target-linux/release/entrotunnel-cli"

docker image inspect et-test >/dev/null 2>&1 || docker build --platform linux/amd64 -t et-test - >/dev/null <<'DOCKER'
FROM debian:bullseye-slim
RUN apt-get update -qq && apt-get install -y -qq iproute2 curl ca-certificates && rm -rf /var/lib/apt/lists/*
DOCKER

echo "==> deploy + start server"
$SSH 'mkdir -p /root/entrotunnel'
scp -q target-linux/release/entrotunnel-server "root@$SERVER:/root/entrotunnel/"
GEN=$($SSH 'cd /root/entrotunnel && ./entrotunnel-server gen-config -c server.toml')
PSK=$(echo "$GEN"   | sed -n 's/^noise_psk : //p')
PTOKEN=$(echo "$GEN" | sed -n 's/^example peer token: \(.*\) -> .*/\1/p')
$SSH "cd /root/entrotunnel && { [ -f server.pid ] && kill \$(cat server.pid) 2>/dev/null || true; }; \
      sleep 1; RUST_LOG=info nohup ./entrotunnel-server -c server.toml run > server.log 2>&1 & \
      echo \$! > server.pid; sleep 2; tail -n 4 server.log"

cat > /tmp/et-split.toml <<EOF
name = "split"
server_host = "$SERVER"
server_port = $PORT
transport = "tcp"
token = "$PTOKEN"
noise_psk = "$PSK"
mode = "global_proxy"
client_name = "split-client"
tun_name = "et0"
http_listen = "127.0.0.1:7890"
tls_skip_verify = false

[[routes]]
target = "1.1.1.1/32"
via = "direct"

[[routes]]
target = "example.com"
via = "direct"
EOF

echo "==> run client (global proxy + split-tunnel rules)"
docker run --rm --platform linux/amd64 \
  --cap-add NET_ADMIN --device /dev/net/tun \
  -e SERVER="$SERVER" \
  -v "$CLI:/usr/local/bin/entrotunnel-cli:ro" \
  -v /tmp/et-split.toml:/etc/client.toml:ro \
  et-test bash -c '
    RUST_LOG=info entrotunnel-cli -c /etc/client.toml run > /tmp/cli.log 2>&1 &
    sleep 5
    echo "    --- split-route log lines ---"; grep "split-route" /tmp/cli.log | sed "s/^/      /" || echo "      (none)"
    echo "    --- ip routes for 1.1.1.1 / 1.0.0.1 ---"
    echo "      1.1.1.1 -> $(ip route get 1.1.1.1 2>/dev/null | head -1)"
    echo "      1.0.0.1 -> $(ip route get 1.0.0.1 2>/dev/null | head -1)"
    DIRECT=$(curl -s --max-time 15 https://1.1.1.1/cdn-cgi/trace | sed -n "s/^ip=//p")
    TUN=$(curl -s --max-time 15 https://1.0.0.1/cdn-cgi/trace | sed -n "s/^ip=//p")
    echo "    egress for 1.1.1.1 (ruled direct): $DIRECT"
    echo "    egress for 1.0.0.1 (tunneled)    : $TUN"
    if [ "$DIRECT" != "$SERVER" ] && [ "$TUN" = "$SERVER" ]; then
      echo "    RESULT: PASS ✅ (ruled dest bypasses tunnel; others still tunneled)"
    else
      echo "    RESULT: FAIL ❌ (direct=$DIRECT tunneled=$TUN, server=$SERVER)"
      echo "    --- client log ---"; sed "s/^/      /" /tmp/cli.log
    fi
  '

echo "==> stop server"
$SSH 'cd /root/entrotunnel && [ -f server.pid ] && kill $(cat server.pid) 2>/dev/null || true'
