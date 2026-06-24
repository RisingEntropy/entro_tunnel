# Testing the global-proxy (TUN) path end-to-end

This verifies the full chain: client TUN → TCP+Noise → server TUN → kernel NAT →
internet, by checking the client's egress IP becomes the server's public IP.

## Topology

```
[ docker container ]                          [ server 141.11.149.77 ]
 entrotunnel-cli                                entrotunnel-server
 TUN et0 = 10.66.0.2/24    ==TCP+Noise:8443==>  TUN et0 = 10.66.0.1/24
 default route -> et0                            ip_forward=1 + MASQUERADE
                                                 egress eth0 -> internet
```

## 1. Build Linux binaries

```bash
./scripts/build-linux.sh            # -> target-linux/release/entrotunnel-{server,cli}
```

## 2. Deploy + start the server

```bash
scp target-linux/release/entrotunnel-server root@141.11.149.77:/root/entrotunnel/
ssh root@141.11.149.77 '
  cd /root/entrotunnel
  ./entrotunnel-server gen-config -c server.toml      # prints noise_psk + a peer token
  ./entrotunnel-server -c server.toml run             # listens tcp 0.0.0.0:8443
'
```

## 3. Run the client in Docker (needs TUN + NET_ADMIN)

```bash
docker run --rm -it --platform linux/amd64 \
  --cap-add NET_ADMIN --device /dev/net/tun \
  -v "$PWD/target-linux/release/entrotunnel-cli:/usr/local/bin/entrotunnel-cli:ro" \
  -v "$PWD/client.toml:/etc/client.toml:ro" \
  debian:bullseye-slim bash -c '
    apt-get update -qq && apt-get install -y -qq iproute2 curl ca-certificates >/dev/null
    echo "egress BEFORE tunnel: $(curl -s https://api.ipify.org)"
    entrotunnel-cli -c /etc/client.toml run &
    sleep 4
    echo "egress THROUGH tunnel: $(curl -s https://api.ipify.org)"   # expect 141.11.149.77
  '
```

`client.toml` must carry the same `noise_psk` and a `token` matching a server
peer record. `mode = "global_proxy"`, `transport = "tcp"`,
`server_host = "141.11.149.77"`, `server_port = 8443`.

## What proves success

`egress THROUGH tunnel` equals the **server's** public IP (141.11.149.77) while
`egress BEFORE tunnel` is the container's own egress — confirming all traffic is
routed through the encrypted tunnel and NATed out the server.
