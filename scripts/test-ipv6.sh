#!/usr/bin/env bash
# Local e2e test for IPv6-through-tunnel + DNS-through-tunnel.
# Runs the server and a CLI client in two containers on a dual-stack Docker
# network. Validates: server NAT66 setup, client v6 address/routes, v6 DNS in
# resolv.conf, and the v6 data path (client → tunnel → server v6 gateway).
set -euo pipefail
cd "$(dirname "$0")/.."
NET=et6net
IMG=debian:bullseye-slim

cleanup() { docker rm -f etsrv etcli >/dev/null 2>&1 || true; docker network rm $NET >/dev/null 2>&1 || true; }
cleanup
trap cleanup EXIT

echo "==> [1/6] dual-stack docker network"
docker network create --ipv6 --subnet 172.31.77.0/24 --subnet fd00:cafe::/64 $NET >/dev/null

echo "==> [2/6] start server container + deps"
docker run -d --name etsrv --network $NET --privileged --device /dev/net/tun \
  -v "$PWD/target-linux/release/entrotunnel-server:/usr/local/bin/entrotunnel-server:ro" \
  $IMG sleep infinity >/dev/null
docker exec etsrv bash -c 'apt-get update -qq >/dev/null && apt-get install -y -qq iproute2 iptables procps >/dev/null'
# Ensure a v6 default route exists so the server treats itself as v6-capable
# (simulates a server with real IPv6 egress).
docker exec etsrv bash -c 'ip -6 route show default | grep -q default || ip -6 route add default via fd00:cafe::1 dev eth0' || true

echo "==> [3/6] gen-config + start server"
GEN=$(docker exec etsrv bash -c 'cd /root && /usr/local/bin/entrotunnel-server -c server.toml gen-config')
echo "$GEN" | sed 's/^/    /'
PSK=$(echo "$GEN"    | sed -n 's/^noise_psk : //p')
PTOKEN=$(echo "$GEN" | sed -n 's/^example peer token: \(.*\) -> .*/\1/p')
SRV4=$(docker inspect -f '{{(index .NetworkSettings.Networks "'$NET'").IPAddress}}' etsrv)
docker exec -d etsrv bash -c 'cd /root && RUST_LOG=info /usr/local/bin/entrotunnel-server -c server.toml run > /var/log/et.log 2>&1'
sleep 3
echo "    --- server log ---"; docker exec etsrv bash -c 'grep -iE "configured|listening|ipv6|nat66" /var/log/et.log | sed "s/^/    /"'
echo "    --- server ip6tables NAT66 ---"; docker exec etsrv bash -c 'ip6tables -t nat -S | grep -i masq | sed "s/^/    /" || echo "    (no NAT66 rule)"'
echo "    --- server TUN v6 addr ---"; docker exec etsrv ip -6 addr show et0 | sed 's/^/    /'

echo "==> [4/6] start client container + deps"
cat > /tmp/c6.toml <<EOF
name = "test6"
server_host = "$SRV4"
server_port = 8443
transport = "tcp"
token = "$PTOKEN"
noise_psk = "$PSK"
mode = "global_proxy"
client_name = "cli6"
tun_name = "et0"
http_listen = "127.0.0.1:7890"
tls_skip_verify = false
EOF
docker run -d --name etcli --network $NET --privileged --device /dev/net/tun \
  -v "$PWD/target-linux/release/entrotunnel-cli:/usr/local/bin/entrotunnel-cli:ro" \
  -v /tmp/c6.toml:/etc/client.toml:ro \
  $IMG sleep infinity >/dev/null
docker exec etcli bash -c 'apt-get update -qq >/dev/null && apt-get install -y -qq iproute2 iputils-ping curl ca-certificates >/dev/null'
docker exec -d etcli bash -c 'RUST_LOG=info entrotunnel-cli -c /etc/client.toml run > /var/log/cli.log 2>&1'
sleep 5

echo "==> [5/6] client v6 config + DNS (the two features)"
echo "    --- client log ---"; docker exec etcli bash -c 'tail -8 /var/log/cli.log | sed "s/^/    /"'
echo "    --- client TUN v6 addr (assigned_ip6) ---"; docker exec etcli ip -6 addr show et0 | grep inet6 | sed 's/^/    /'
echo "    --- client v6 default-capture routes ---"; docker exec etcli ip -6 route show | grep -E '::/1|8000::' | sed 's/^/    /'
echo "    --- client resolv.conf (DNS through tunnel; expect v4 + v6) ---"; docker exec etcli cat /etc/resolv.conf | sed 's/^/    /'

echo "==> [6/6] IPv6 DATA PATH: client ping6 the server v6 gateway fd66::1 through the tunnel"
if docker exec etcli ping6 -c 3 -W 3 fd66::1 | sed 's/^/    /'; then
  echo "    ✅ PASS: IPv6 packets traverse the tunnel (TUN capture → frame → server demux → reply)"
else
  echo "    ❌ v6 tunnel path failed"
fi
echo "==> best-effort real v6 internet egress (needs host/server v6):"
docker exec etcli bash -c 'curl -6 -s --max-time 8 https://api64.ipify.org && echo "" || echo "    (no real v6 egress here — expected without host IPv6; works on a v6-capable server)"' | sed 's/^/    /'
