//! Wire-shared configuration enums and small crypto-material helpers.
//!
//! Server- and client-specific config structs live in their own crates; only
//! the enums that appear *on the wire* (and therefore must agree) live here.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Which transport a link uses. Selectable per server listener and per client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransportKind {
    /// Raw TCP, encrypted with Noise.
    Tcp,
    /// WebSocket over TLS (WSS).
    Ws,
    /// QUIC (native TLS 1.3).
    Quic,
}

impl fmt::Display for TransportKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TransportKind::Tcp => write!(f, "tcp"),
            TransportKind::Ws => write!(f, "ws"),
            TransportKind::Quic => write!(f, "quic"),
        }
    }
}

/// What the client is asking the server to do for this session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionMode {
    /// Capture ALL traffic via TUN and egress through the server.
    GlobalProxy,
    /// Set the OS-wide system proxy to the local listener: proxy-aware apps go
    /// through the server, others are unaffected (between HTTP-proxy and TUN).
    SystemProxy,
    /// Only proxy traffic sent to the local HTTP/SOCKS listener.
    HttpProxy,
    /// Virtual LAN: reach other peers on the same server by IP.
    Vpn,
}

impl SessionMode {
    /// The mode as seen by the server on the wire. `SystemProxy` is a purely
    /// client-side concern (it just also flips the OS proxy switch); on the link
    /// it behaves exactly like `HttpProxy`, so the server need not know about it.
    pub fn wire(self) -> SessionMode {
        match self {
            SessionMode::SystemProxy => SessionMode::HttpProxy,
            other => other,
        }
    }

    /// Whether this mode multiplexes proxy streams (vs. forwarding raw packets).
    pub fn is_stream(self) -> bool {
        matches!(self, SessionMode::HttpProxy | SessionMode::SystemProxy)
    }
}

impl fmt::Display for SessionMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SessionMode::GlobalProxy => write!(f, "global_proxy"),
            SessionMode::SystemProxy => write!(f, "system_proxy"),
            SessionMode::HttpProxy => write!(f, "http_proxy"),
            SessionMode::Vpn => write!(f, "vpn"),
        }
    }
}

/// Generate a fresh base64-encoded 32-byte pre-shared key (`noise_psk`).
pub fn generate_psk() -> String {
    use base64::Engine;
    use rand::RngCore;
    let mut key = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut key);
    base64::engine::general_purpose::STANDARD.encode(key)
}

/// Decode a base64 `noise_psk` into 32 raw bytes.
pub fn parse_psk(s: &str) -> crate::Result<[u8; 32]> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(s.trim())
        .map_err(|e| crate::Error::Config(format!("invalid psk base64: {e}")))?;
    if bytes.len() != 32 {
        return Err(crate::Error::Config(format!(
            "psk must be 32 bytes, got {}",
            bytes.len()
        )));
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(&bytes);
    Ok(key)
}

/// Generate a random client token (used as the pre-shared identity).
pub fn generate_token() -> String {
    uuid::Uuid::new_v4().to_string()
}
