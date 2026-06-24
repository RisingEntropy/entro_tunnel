# EntroTunnel

A self-hosted VPN / proxy system written in Rust.

- **Server** — runs on your box, listens on one or more configurable
  protocol/port combinations, and ships with a built-in web admin panel.
- **Client** — a [Tauri](https://tauri.app) desktop app (Clash-Verge style UI)
  for macOS, Windows and Linux.

## Features

1. **Global proxy** — a virtual NIC (TUN) captures *all* of the client's
   traffic and routes it out through the server.
2. **System proxy** — runs the local proxy *and* sets it as the OS-wide system
   proxy, so proxy-aware apps use the tunnel automatically while everything else
   is untouched. An intensity between HTTP-proxy and TUN (à la Clash Verge); the
   OS setting is restored on disconnect. (macOS `networksetup`, Linux GNOME
   `gsettings`.)
3. **HTTP proxy** — a local HTTP/SOCKS listener proxies *only* explicitly
   proxied traffic through the server (no TUN required).
4. **VPN (virtual LAN)** — every client connected to the same server shares a
   virtual subnet and can reach the others by IP. Each client's IP is
   configurable both client-side (requested) and server-side (assigned/pinned).

Mode is a **local connection choice** (picked on the client after selecting a
server), not part of a profile — see "Sharing configs" below.

## Transports & encryption

The link between client and server can run over **TCP**, **WebSocket** or
**QUIC**, selectable per server listener. **No traffic is ever sent in the
clear.** The encryption strategy is hybrid:

| Transport | Encryption                                              |
|-----------|---------------------------------------------------------|
| QUIC      | native TLS 1.3                                          |
| WebSocket | TLS 1.3 (WSS)                                           |
| TCP       | Noise protocol (`Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s`) |

Client identity is a **pre-shared token** (Clash-style). The server matches the
token to a configured peer record and assigns/pins that peer's virtual IP.

## Multiple servers

A client profile can list several servers and switch between them. Each server
carries its own endpoint, transport, token and crypto; `mode`, TUN and routing
rules are shared:

```toml
selected_server = "tokyo"
mode = "global_proxy"

[[servers]]
name = "tokyo"
host = "203.0.113.10"
port = 8443
transport = "tcp"
token = "..."
noise_psk = "..."

[[servers]]
name = "frankfurt"
host = "198.51.100.7"
port = 8444
transport = "ws"
token = "..."
noise_psk = "..."
tls_skip_verify = true
```

```bash
entrotunnel-cli servers                 # list servers, show the active one
entrotunnel-cli --server frankfurt run  # connect using a specific server
```

In the GUI, the **Dashboard** has a server picker and the **Profiles** editor
manages the server list. A single-server config (flat `server_host`/`server_port`/…
fields, no `[[servers]]`) still works unchanged.

## Split-tunnel routing rules

In global-proxy / VPN mode you can route specific destinations out a chosen
interface instead of the virtual NIC. Add `[[routes]]` entries to the client
profile; each is installed as a more-specific route, so it wins over the
tunnel's catch-all:

```toml
[[routes]]
target = "192.168.0.0/16"   # domain ("example.com") or IP / CIDR (IPv4)
via    = "direct"           # bypass the tunnel via the host's default NIC

[[routes]]
target = "10.8.0.5"
via    = "eth1"             # send out a specific NIC (optional `gateway = "..."`)

[[routes]]
target = "internal.corp"
via    = "tunnel"           # force through the virtual NIC
```

`via` is `"direct"`, `"tunnel"`, or an interface name. Domains are resolved to
IPv4 addresses at connect time. See [docs/TESTING.md](docs/TESTING.md) for the
`scripts/test-split-tunnel.sh` verification.

## Sharing configs (export / import)

A **profile is server config only** — endpoints, tokens and crypto. *Which mode*
to run (global proxy / HTTP proxy / VPN) and its parameters (TUN name, HTTP
listen, routes) are chosen locally on the client's connection screen, not baked
into the profile. This mirrors Clash Verge: a profile/subscription is "where to
connect", the mode is "how to use it right now".

Because a profile carries no device-specific mode, it is portable. The server
admin panel can **export** a peer as a single `entro://…` link; the client
**imports** it by paste:

```bash
# CLI: import a link the admin exported, then connect
entrotunnel-cli import "entro://…" --mode global_proxy
entrotunnel-cli run
```

In the GUI, **Profiles → Import** takes the pasted link; **Profiles → Export**
produces one to share.

## Web admin & statistics

The server ships a single-file web admin (axum). It lists peers, adds/removes
them, exports their configs, and shows **live, in-memory statistics**: per-client
traffic, online clients grouped by mode (VPN nodes vs proxy clients), and a
throughput timeline — rendered as donut + area charts. Stats are never written to
disk; they cover the running process only.

Bind it to localhost (default) and reach it over an SSH tunnel, or front it with
a reverse proxy. The repo's [deploy/](deploy/) shows nginx terminating TLS for a
domain and proxying both the WebSocket tunnel and the admin panel (at `/panel/`).

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) and
[docs/PROTOCOL.md](docs/PROTOCOL.md) for the full design.

## Repository layout

```
EntroTunnel/
├── core/                 # 内核 — shared, platform-agnostic engine
│   ├── core/             entrotunnel-core   — protocol, crypto, transports, TUN, config
│   └── client-core/      entrotunnel-client — client engine: connection, proxy, chain, modes
├── server/               entrotunnel-server — server binary + web admin (axum)
├── desktop/
│   ├── client-tauri/     Tauri desktop app (React + TS frontend, src-tauri backend)
│   └── cli/              entrotunnel-cli    — headless client
├── mobile/
│   └── android/          Android app (Jetpack Compose) + rust/ JNI core
└── docs/                 architecture & wire-protocol specs
```

`core/` is reused unchanged by every client — desktop (Tauri), CLI and Android
(via a JNI `cdylib`). The Tauri `src-tauri` and Android `rust` crates are
detached sub-workspaces, so the root `cargo check` stays fast.

## Building

```bash
# Core / server / client engine (the main Cargo workspace)
cargo check
cargo run -p entrotunnel-server -- --config server.toml

# Desktop client (separate workspace) — uses Yarn
cd desktop/client-tauri
yarn install
yarn tauri dev
```

> **Status:** working. Global-proxy (TUN), HTTP-proxy and VPN modes, all three
> transports, split-tunnel routing, multi-server profiles, latency probing and
> the web admin are implemented and deployed. Per-platform TUN backends exist for
> Linux, macOS (utun) and Windows (wintun).

## License

MIT OR Apache-2.0
