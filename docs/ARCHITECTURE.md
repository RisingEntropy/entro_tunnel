# EntroTunnel — Architecture

## 1. Components

```
        ┌─────────────────────────── client (Tauri app) ───────────────────────────┐
        │  React UI  ──tauri::command──►  entrotunnel-client (engine)               │
        │                                   ├─ connection manager                   │
        │                                   ├─ proxy (HTTP / SOCKS5 local listener) │
        │                                   ├─ tun device (macOS / Win / Linux)     │
        │                                   └─ mode controller                      │
        └───────────────────────────────────────┬───────────────────────────────────┘
                                                 │  encrypted link
                            TCP+Noise / WSS / QUIC (TLS 1.3)
                                                 │
        ┌────────────────────────────────────────┴──────────────────────────────────┐
        │  entrotunnel-server                                                         │
        │   ├─ listeners (one per configured protocol/port)                           │
        │   ├─ session manager (token → peer, virtual-IP table)                       │
        │   ├─ router (peer↔peer VPN switching by dest IP)                            │
        │   ├─ egress (global-proxy / http-proxy → real internet)                     │
        │   └─ web admin (axum REST API + embedded SPA)                               │
        └─────────────────────────────────────────────────────────────────────────────┘
```

All three of `core`, `server`, `client-core` live in one Cargo workspace. The
Tauri app is a *separate* workspace (its `src-tauri/Cargo.toml` has its own
`[workspace]` table) that path-depends on `entrotunnel-core` and
`entrotunnel-client`. This keeps `cargo check` at the repo root fast and free of
Tauri's heavy build graph.

## 2. Layered link stack

Every client↔server link is the same set of layers, regardless of feature:

```
┌─────────────────────────────────────────────┐
│ application frames (Frame enum)              │  control + data, bincode-encoded
├─────────────────────────────────────────────┤
│ MessageChannel  (send/recv length-bounded)   │  trait object: Box<dyn MessageChannel>
├─────────────────────────────────────────────┤
│ encryption                                   │  Noise (raw TCP)  OR  TLS 1.3 (WSS/QUIC)
├─────────────────────────────────────────────┤
│ transport                                    │  TCP  /  WebSocket  /  QUIC
└─────────────────────────────────────────────┘
```

- A **`MessageChannel`** (`core::transport::MessageChannel`) is the unifying
  abstraction: an ordered, reliable, *already-encrypted* stream of byte
  messages. Transports differ only in how they produce one.
  - TCP + TLS, QUIC bi-stream → a generic length-delimited framing over an
    `AsyncRead + AsyncWrite` (`LengthDelimited`).
  - WebSocket → binary WS messages map 1:1 to channel messages.
  - Raw TCP → `NoiseChannel`: each message is one Noise transport message
    (ciphertext), length-prefixed.
- On top of a `MessageChannel`, `core::protocol::FrameTransport` serializes the
  `Frame` enum (handshake + data). This is the only thing the server session
  loop and the client engine speak.

## 3. Encryption choices (per the hybrid decision)

- **QUIC / WebSocket** ride on **TLS 1.3** (`rustls`). The server presents a
  certificate (configured PEM, or a self-signed one generated at first start
  and persisted). The client either pins that certificate / its own CA, or — for
  quick self-hosted setups — runs in `insecure_skip_verify` mode (explicit,
  logged, off by default). Token auth still happens at the `Hello` layer.
- **Raw TCP** uses the **Noise** pattern
  `Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s`:
  - ephemeral X25519 keys → forward secrecy,
  - a 32-byte pre-shared key (`noise_psk`, in both server and client config)
    mixed in at `psk0` → MITM resistance + rejects anyone without the PSK,
  - ChaCha20-Poly1305 AEAD for every transport message.

Two distinct secrets, two jobs: the **`noise_psk`** authenticates the *channel*
(are you allowed to talk to this server at all), the per-client **token**
authenticates the *peer* (which virtual IP / ACLs you get). For TLS transports
the channel is authenticated by the certificate instead of the PSK.

## 4. Sessions, peers and IP assignment

The server keeps a registry of **peers** (`server::config::PeerConfig`), each
with a token and a *pinned* virtual IPv4. On `Hello`:

1. look up the peer by token (reject if unknown / disabled),
2. resolve the virtual IP: pinned IP wins; otherwise the client's
   `requested_ip` if free and inside the pool; otherwise next free address,
3. register the live `Session` in the IP→session table,
4. reply `Welcome { assigned_ip, prefix_len, gateway, mtu, dns }`.

The **router** is just a `DashMap<Ipv4Addr, SessionHandle>`. For VPN traffic the
server reads the dest IP out of each forwarded IP packet and hands it to the
matching session (or to **egress** if the dest is outside the virtual subnet and
the peer is allowed global routing).

## 5. Feature data paths

| Mode          | Client side                                  | Server side                              |
|---------------|----------------------------------------------|------------------------------------------|
| Global proxy  | TUN captures all IP packets → `Frame::Packet`| egress NAT to the internet; replies back |
| HTTP proxy    | local HTTP/SOCKS listener → `Frame::StreamOpen/StreamData` | dial target, pump bytes both ways |
| VPN           | TUN captures packets for the virtual subnet  | router switches packets between peers     |

Global-proxy and VPN both ride the **packet path** (`Frame::Packet`, raw IPv4);
they differ only in the client's routing table and the server's
route/egress decision. HTTP proxy rides the **stream path**
(`Frame::StreamOpen/Data/Close`) and needs no TUN.

## 6. Configuration

- Server: `server.toml` — listeners (protocol + bind addr + TLS material),
  virtual subnet/pool, peer list, egress policy. See `server::config`.
- Client: `client.toml` (and the Tauri app's stored profile) — server endpoint,
  transport, token, `noise_psk`, mode, requested IP. See `client_core::config`.

Both are plain `serde` + `toml`; the web admin and the Tauri UI read/write the
same structs.

## 7. Status / TODO map

Implemented in the scaffold: workspace, crate boundaries, error/config types,
`Frame` codec, `MessageChannel` trait, Noise channel, TLS config helpers,
transport connect/listen entry points, server skeleton (listeners + session +
router + web admin REST + embedded SPA), client engine skeleton, Tauri shell.

Marked `TODO(impl)` (return `Error::NotImplemented` / `todo!()` where unreachable
at startup): real TUN packet I/O per OS, server egress NAT, proxy stream
pumping, QUIC/WS channel inner wiring details, system route table manipulation.
