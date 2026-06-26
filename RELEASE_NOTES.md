A self-hosted, end-to-end-encrypted VPN / proxy written in Rust. One engine powers a **server** (with a web admin panel), a **desktop GUI** (Tauri — macOS / Windows / Linux), a **CLI**, and an **Android** client. No plaintext anywhere — you run every hop yourself.

## What's new in v1.2
- 🐛 **Server reliability fix** — connection handshakes now run *off* the accept loop under a timeout. Previously one peer that stalled mid-handshake (a dropped client, a port scanner, a cloud/GFW probe) could wedge the listener: the process stayed up but stopped accepting new connections. Fixed for all transports (TCP / WS / QUIC).
- 🎨 **Desktop UI overhaul** — light / dark themes with a sidebar toggle (persisted, follows system preference); a fuller Clash-Verge-style sidebar with logo, name, and a live traffic curve; proper [Lucide](https://lucide.dev) icons; refined typography, cards, and switches.
- 🚦 **Latency colouring by threshold** — green < 250 ms, amber 250–500 ms, red > 500 ms (and timeouts).

## Highlights
- 🔗 **Multi-hop proxy chain** — relay through several of your own servers
- 🌐 **Full-tunnel IPv6 (NAT66)** + DNS over the tunnel
- 🛡️ **IPv6 kill-switch** — no v6 leaks on IPv4-only exits
- ✂️ **Split-tunnel** (whitelist / blacklist)
- 🚇 Three transports: **TCP+Noise**, **WebSocket/TLS (WSS)**, **QUIC**
- 🖥️ Four modes: Global proxy (TUN), System proxy, HTTP proxy, VPN peer LAN

## Connection modes
- **Global proxy (TUN)** — a virtual NIC captures *all* traffic; the OS routes everything through the tunnel.
- **System proxy** — sets the OS proxy to a local listener (à la Clash Verge); sits between HTTP-proxy and TUN.
- **HTTP proxy** — a local HTTP proxy apps point at; domains are resolved **server-side**, so DNS travels the tunnel too.
- **VPN (peer LAN)** — devices on the *same* server reach each other by virtual IP. Any proxy mode can additionally "join the VPN".

## Transports & crypto
- **TCP** — raw TCP encrypted with **Noise** (`Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s`).
- **WebSocket** — WSS / TLS 1.3; works behind nginx + Let's Encrypt (looks like ordinary HTTPS).
- **QUIC** — native TLS 1.3.
- Auth = a **pre-shared token** (Clash-style); the server pins each peer's virtual IP. Crypto provider pinned to `ring`.

## 🔗 Proxy chain (multi-hop)
Relay your traffic through **two or more of your own servers** before it exits:

```
you → S1 → S2 → … → Sn → internet
```

- Each hop is independently encrypted (Noise, plus WS/TLS-over-relay for hops behind nginx). The egress **mode runs at the last hop**.
- Chosen **per connection** on the Home screen — pick the ordered list of servers from your active profile. Fewer than 2 hops = a normal single-server connection.
- Implemented entirely **client-side**: chaining needs **no server changes** (each server just relays a stream to the next). QUIC can only be the first hop.
- When you also join the VPN, the **LAN is the first hop**.

## 🌐 IPv6 handling
EntroTunnel is **dual-stack**. When a server has working IPv6 egress it advertises a ULA subnet (`fd66::/64`) and sets up **NAT66**:

- The server derives each client's IPv6 from its pinned IPv4 (e.g. `10.66.0.2` → `fd66::a42:2`), brings up the v6 gateway on its TUN, enables v6 forwarding, and installs `ip6tables` MASQUERADE.
- In **global-proxy** mode the client adds the v6 address and routes `::/1` + `8000::/1` through the tunnel, so **IPv6 egresses via your server** — no native-v6 bypass.
- **DNS travels the tunnel** (v4 + v6 resolvers) so lookups resolve from the server's vantage point — Linux via `/etc/resolv.conf`, macOS via `networksetup`. HTTP-proxy mode resolves domains server-side.
- Servers **without** IPv6 egress stay IPv4-only (no v6 advertised) — leaks there are handled by the kill-switch below.
- Raw `Frame::Packet` payloads are version-agnostic; the server demuxes by IP version (v4 → dst bytes 16..20, v6 → 24..40).

## 🛡️ IPv6 kill-switch (leak protection)
A classic VPN pitfall: on an **IPv4-only** exit, your machine's *native* IPv6 (e.g. an ISP `240e::` address) keeps routing directly and **leaks your real IP / location** even though IPv4 is tunneled.

EntroTunnel blocks this. In global-proxy mode, when the server is v4-only, the client **disables native IPv6** for the duration of the connection:

- **Linux** — installs `unreachable` routes for all global v6 (`::/1` + `8000::/1`) so apps fail fast and fall back to the tunneled IPv4. Link-local / on-link stay intact (NDP/SLAAC keep working).
- **macOS** — turns IPv6 off on the active network service (`networksetup -setv6off`), restored automatically on disconnect.
- On an IPv6-capable server, v6 is tunneled instead (no kill-switch needed).

**Toggleable** — on by default (the safe choice). Turn it off under **Settings → IPv6 leak protection** to keep native IPv6 on v4-only exits.

## ✂️ Split-tunnel
Per-destination rules send specific targets out a chosen interface instead of the tunnel:
- **Blacklist** (default): tunnel everything, carve out direct exceptions.
- **Whitelist**: tunnel only the listed destinations; everything else stays direct.

Rules accept domains (resolved at connect time) or IP / CIDR.

## Profiles & admin
- A **Profile** = server config only (it never carries a mode); mode / TUN / etc. are device-local choices made at connect time.
- Multi-server profiles with **latency probing**; import/export via a portable `entro://` link or a one-file TOML bundle.
- A **web admin panel** (axum) on the server: peers, live traffic, virtual-IP pinning.

## Downloads (attached below)
- **macOS** (Apple Silicon + Intel, universal) — `.dmg`
- **Windows** — `.msi` / NSIS `.exe`
- **Linux** — `.deb` / `.AppImage`
- **Server + CLI** binaries for macOS / Windows / Linux

These are **unsigned** builds. On macOS, right-click the app → **Open** the first time (or `xattr -dr com.apple.quarantine /Applications/EntroTunnel.app`). TUN modes need admin / Touch-ID elevation. (The Android client builds from source — see the README.)

## Notes & limitations
- Per-OS runtime support differs (Linux is the gold path for TUN/VPN; Windows TUN routing is in progress) — see the README matrix.
- A datacenter exit IP is still a datacenter IP: VPN detection by IP reputation is independent of tunnel mode.
- Real IPv6 *internet* egress requires a server that actually has IPv6.
