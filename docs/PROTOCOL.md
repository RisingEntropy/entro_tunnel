# EntroTunnel — Wire Protocol

Everything below sits **inside** the encrypted layer (Noise or TLS). Bytes on
the physical wire are always ciphertext.

## 1. Message framing (`MessageChannel`)

A `MessageChannel` delivers ordered, reliable byte *messages*. How they are
framed depends on the transport:

- **Length-delimited** (TCP+TLS, QUIC streams): `u32` big-endian length prefix
  followed by that many plaintext bytes. Max message size is
  `MAX_MESSAGE_LEN` (default 256 KiB) to bound allocation.
- **WebSocket**: each binary WS frame *is* one message. (Text/ping/pong frames
  are control-only and skipped by the channel.)
- **Noise** (raw TCP): `u16` big-endian length prefix + one Noise transport
  message (ciphertext). Each plaintext payload is ≤ 65535 − 16 (AEAD tag), so
  larger app frames are split before encryption.

## 2. Application frames (`Frame`)

App frames are `bincode`-encoded `Frame` enum values, carried as
`MessageChannel` messages.

```rust
enum Frame {
    // ---- control ----
    Hello(Hello),                 // client → server, first frame
    Welcome(Welcome),             // server → client, on accept
    Reject { reason: String },    // server → client, on refuse (then close)
    Ping,                         // keepalive (either direction)
    Pong,

    // ---- packet path (global proxy + VPN) ----
    Packet(Vec<u8>),              // one raw IPv4 packet

    // ---- stream path (HTTP proxy) ----
    StreamOpen  { id: u32, target: TargetAddr }, // client → server: dial target
    StreamData  { id: u32, data: Vec<u8> },      // both directions
    StreamClose { id: u32, error: Option<String> },
}
```

`TargetAddr` is either `Ip(SocketAddr)` or `Domain(String, u16)` (domain
resolution happens server-side so DNS also goes through the tunnel).

## 3. Handshake

```
client                                   server
  │  ── Hello { version, token,            │
  │            mode, requested_ip } ──────►│  look up peer by token
  │                                        │  resolve virtual IP
  │                                        │  register session
  │  ◄──────── Welcome { session_id,       │
  │            assigned_ip, prefix_len,     │
  │            gateway, mtu, dns,           │
  │            assigned_ip6, … } ───────────│
  │                                        │
  │  (or ◄── Reject { reason } ── then close)
  │                                        │
  │ ════════ data frames (packet / stream) ═══════►
  │ ◄═══════════════════════════════════════════════
```

`Hello.version` is `PROTOCOL_VERSION` (`u16`). Mismatched major version →
`Reject`.

```rust
struct Hello {
    version: u16,
    token: String,
    mode: SessionMode,            // GlobalProxy | HttpProxy | Vpn
    requested_ip: Option<Ipv4Addr>,
    client_name: Option<String>,  // shown in the admin panel
}

struct Welcome {
    session_id: Uuid,
    assigned_ip: Ipv4Addr,
    prefix_len: u8,               // virtual subnet, e.g. /24
    gateway: Ipv4Addr,            // server's virtual IP
    mtu: u16,
    dns: Vec<Ipv4Addr>,
    // --- v2 (dual-stack), present only when the server has IPv6 egress ---
    assigned_ip6: Option<Ipv6Addr>, // ULA, derived from the v4 (fd66::a42:2)
    prefix6: u8,                     // e.g. /64
    gateway6: Option<Ipv6Addr>,      // server's virtual IPv6 (fd66::1)
    dns6: Vec<Ipv6Addr>,             // v6 resolvers, also routed through the tunnel
}
```

**IPv6 (NAT66).** When the server has a ULA subnet (`subnet6`, default `fd66::/64`)
*and* detects a working IPv6 default route, it brings up `gateway6` on its TUN,
enables v6 forwarding, and installs `ip6tables` MASQUERADE — then hands each
client a v6 derived from its pinned v4 (the v4 goes in the low 32 bits, so it is
stable and collision-free). The client adds the v6 address to its TUN and routes
`::/1` + `8000::/1` through it (full-tunnel), so IPv6 egresses via the server. The
`Frame::Packet` payload is unchanged — a raw IP packet, demuxed by version nibble
(4 → v4 dest bytes 16..20, 6 → v6 dest bytes 24..40). Servers without IPv6 egress
simply omit the v6 fields and stay v4-only.

**DNS through the tunnel.** In global-proxy (full-tunnel) mode the client points
the OS resolver at `dns` + `dns6`; because those resolver IPs are inside the
captured route range, every query travels through the tunnel and is answered from
the server's vantage point (Linux: `/etc/resolv.conf`; macOS: `networksetup`).
HTTP-proxy mode already resolves domains server-side (`TargetAddr::Domain`).

## 4. Routing rules (server)

For each `Frame::Packet` from peer *P* with destination IP *D*:

1. *D* ∈ virtual subnet and a live session owns *D* → forward the packet to that
   session (VPN peer-to-peer).
2. *D* == gateway → packet is for the server itself (e.g. virtual DNS).
3. *D* outside the virtual subnet → if *P* is allowed global routing
   (`PeerConfig.allow_global`), hand to **egress** (NAT to the internet),
   else drop.

For `Frame::StreamOpen` the server dials `target` (always allowed only if
`allow_http_proxy`) and pumps bytes via `StreamData` until either side closes.

## 5. Keepalive & teardown

- Either side may send `Ping`; the peer replies `Pong`. Default idle interval
  15 s, dead-link timeout 45 s.
- Closing the `MessageChannel` (TCP FIN, WS close, QUIC close) ends the session;
  the server unregisters the virtual IP and tears down any proxied streams.

## 6. Constants

| name               | value     |
|--------------------|-----------|
| `PROTOCOL_VERSION` | `1`       |
| `MAX_MESSAGE_LEN`  | `262144`  |
| `DEFAULT_MTU`      | `1380`    |
| `KEEPALIVE_SECS`   | `15`      |
| `DEAD_LINK_SECS`   | `45`      |
| Noise pattern      | `Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s` |
