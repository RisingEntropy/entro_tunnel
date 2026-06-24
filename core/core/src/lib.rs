//! `entrotunnel-core` — shared types for the EntroTunnel client and server.
//!
//! The crate is organised in layers (see `docs/ARCHITECTURE.md`):
//!
//! * [`transport`] — pluggable encrypted byte-message channels (TCP+Noise,
//!   WSS, QUIC) behind the [`transport::MessageSink`] / [`transport::MessageStream`]
//!   traits.
//! * [`crypto`] — Noise handshake + TLS material helpers.
//! * [`protocol`] — the application [`protocol::Frame`] enum and its codec on top
//!   of a message channel.
//! * [`config`] — wire-shared config enums (`TransportKind`, `SessionMode`).

pub mod config;
pub mod crypto;
pub mod error;
pub mod protocol;
pub mod transport;
pub mod tun;

pub use error::{Error, Result};

/// Bumped on incompatible wire changes; checked in the `Hello` handshake.
/// v2: dual-stack — `Welcome` carries IPv6 (NAT66) params + v6 DNS, `PeerInfo`
/// carries a v6 address. `Hello` is unchanged, so an old server still parses it
/// and rejects v2 clients with a clean "protocol version mismatch".
pub const PROTOCOL_VERSION: u16 = 2;
/// Upper bound on a single length-delimited message (anti-DoS allocation cap).
pub const MAX_MESSAGE_LEN: usize = 256 * 1024;
/// Default tunnel MTU. Conservative to survive PPPoE / extra encapsulation.
pub const DEFAULT_MTU: u16 = 1380;
/// Idle keepalive interval.
pub const KEEPALIVE_SECS: u64 = 15;
/// Link is considered dead after this long without traffic.
pub const DEAD_LINK_SECS: u64 = 45;
/// Noise pattern used for raw-TCP encryption (PSK-authenticated channel).
pub const NOISE_PATTERN: &str = "Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s";
