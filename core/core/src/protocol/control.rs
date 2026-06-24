//! Handshake / control message payloads. See `docs/PROTOCOL.md` §3.

use crate::config::SessionMode;
use serde::{Deserialize, Serialize};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use uuid::Uuid;

/// First frame sent by the client after the encrypted channel is up.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    pub version: u16,
    /// Pre-shared identity token (Clash-style). Maps to a server peer record.
    pub token: String,
    pub mode: SessionMode,
    /// Optional client-requested virtual IP (server may override / pin).
    pub requested_ip: Option<Ipv4Addr>,
    /// Friendly name shown in the admin panel.
    pub client_name: Option<String>,
    /// Join this server's VPN (peer LAN) *in addition to* whatever `mode` does.
    /// Lets e.g. a proxy-mode client also reach other devices by virtual IP.
    /// Always implied true for [`SessionMode::Vpn`]; the server treats a session
    /// as a VPN member when `mode == Vpn || join_vpn`.
    #[serde(default)]
    pub join_vpn: bool,
}

/// Server's acceptance reply, carrying the assigned virtual-network config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Welcome {
    pub session_id: Uuid,
    pub assigned_ip: Ipv4Addr,
    pub prefix_len: u8,
    pub gateway: Ipv4Addr,
    pub mtu: u16,
    pub dns: Vec<Ipv4Addr>,
    /// IPv6 (NAT66) parameters — present only when the server has working IPv6
    /// egress and a configured ULA subnet. `None`/empty disables v6 on the client.
    #[serde(default)]
    pub assigned_ip6: Option<Ipv6Addr>,
    #[serde(default)]
    pub prefix6: u8,
    #[serde(default)]
    pub gateway6: Option<Ipv6Addr>,
    /// IPv6 DNS resolvers (routed through the tunnel like the v4 ones).
    #[serde(default)]
    pub dns6: Vec<Ipv6Addr>,
}

/// One VPN peer, as reported to a client in [`super::Frame::PeerList`]. Carries
/// just what the client shows: the peer's virtual IP(s) and its friendly name.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerInfo {
    pub ip: Ipv4Addr,
    pub name: String,
    /// The peer's virtual IPv6, when the server runs dual-stack.
    #[serde(default)]
    pub ip6: Option<Ipv6Addr>,
}

/// Destination for a proxied stream (HTTP-proxy mode). Domains are resolved
/// server-side so DNS also travels through the tunnel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TargetAddr {
    Ip(SocketAddr),
    Domain(String, u16),
}

impl std::fmt::Display for TargetAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TargetAddr::Ip(a) => write!(f, "{a}"),
            TargetAddr::Domain(h, p) => write!(f, "{h}:{p}"),
        }
    }
}
