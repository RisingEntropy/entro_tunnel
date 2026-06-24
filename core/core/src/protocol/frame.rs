//! The application-level [`Frame`] enum carried over a `MessageChannel`.

use super::control::{Hello, PeerInfo, TargetAddr, Welcome};
use serde::{Deserialize, Serialize};

/// One application frame. Encoded with `bincode` into a single channel message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Frame {
    // ---- control ----
    Hello(Hello),
    Welcome(Welcome),
    Reject { reason: String },
    Ping,
    Pong,

    // ---- packet path (global proxy + VPN) ----
    /// One raw IPv4 packet captured from / destined for a TUN device.
    Packet(Vec<u8>),

    // ---- stream path (HTTP proxy) ----
    StreamOpen { id: u32, target: TargetAddr },
    StreamData { id: u32, data: Vec<u8> },
    StreamClose { id: u32, error: Option<String> },

    // ---- VPN peer discovery ----
    // NOTE: appended at the end so existing variant indices are unchanged
    // (bincode encodes an enum by its variant position).
    /// Client → server: "who else is on this VPN?"
    GetPeers,
    /// Server → client: the VPN peers currently on this server (excludes the
    /// requester). Sent in reply to [`Frame::GetPeers`].
    PeerList { peers: Vec<PeerInfo> },
}
