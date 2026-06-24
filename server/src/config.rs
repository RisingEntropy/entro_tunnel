//! Server configuration (`server.toml`). Editable by hand or via the web admin.

use entrotunnel_core::config::{generate_psk, generate_token, TransportKind};
use entrotunnel_core::{Error, Result};
use serde::{Deserialize, Serialize};
use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::Path;

fn yes() -> bool {
    true
}

// IPv6 (NAT66) defaults. A unique-local (ULA) subnet by default; the server only
// actually offers v6 to clients when it also detects working IPv6 egress.
fn default_subnet6() -> Option<String> {
    Some("fd66::/64".to_string())
}
fn default_gateway6() -> Option<Ipv6Addr> {
    "fd66::1".parse().ok()
}
fn default_dns6() -> Vec<Ipv6Addr> {
    // Google + Cloudflare public IPv6 resolvers.
    ["2001:4860:4860::8888", "2606:4700:4700::1111"]
        .iter()
        .filter_map(|s| s.parse().ok())
        .collect()
}

/// One listening socket: a transport bound to an address.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListenerConfig {
    pub transport: TransportKind,
    /// `addr:port`, e.g. `0.0.0.0:8443`.
    pub bind: String,
    /// For `ws`: terminate TLS here (WSS, default) or accept plain WS when a
    /// front proxy (nginx) terminates TLS. Ignored for tcp/quic.
    #[serde(default = "yes")]
    pub tls: bool,
}

/// Virtual network parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkConfig {
    /// Virtual subnet in CIDR, e.g. `10.66.0.0/24`.
    pub subnet: String,
    /// Server's own virtual IP (the gateway peers route through).
    pub gateway: Ipv4Addr,
    pub mtu: u16,
    /// DNS servers handed to clients (reachable through the tunnel).
    pub dns: Vec<Ipv4Addr>,
    /// Server-side TUN device name.
    #[serde(default = "default_tun")]
    pub tun_name: String,
    /// Real egress interface for NAT (auto-detected from the default route if
    /// unset).
    #[serde(default)]
    pub egress_iface: Option<String>,
    /// IPv6 ULA subnet for NAT66 egress, e.g. `fd66::/64`. The server only hands
    /// v6 to clients when it ALSO has working IPv6 egress. `None` disables v6.
    #[serde(default = "default_subnet6")]
    pub subnet6: Option<String>,
    /// Server's own virtual IPv6 (the v6 gateway), e.g. `fd66::1`.
    #[serde(default = "default_gateway6")]
    pub gateway6: Option<Ipv6Addr>,
    /// IPv6 DNS resolvers handed to clients (routed through the tunnel).
    #[serde(default = "default_dns6")]
    pub dns6: Vec<Ipv6Addr>,
}

fn default_tun() -> String {
    "et0".to_string()
}

/// Crypto material.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityConfig {
    /// base64 32-byte Noise PSK (raw-TCP channel auth). Shared with clients.
    pub noise_psk: String,
    /// PEM cert/key for TLS transports. If unset, a self-signed pair is
    /// generated and saved next to the config on first start.
    #[serde(default)]
    pub tls_cert_path: Option<String>,
    #[serde(default)]
    pub tls_key_path: Option<String>,
}

/// Web admin panel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebConfig {
    pub bind: String,
    /// Bearer token required by the admin API (sent as `?token=` or
    /// `Authorization: Bearer`).
    pub admin_token: String,
}

/// A client peer record (matched by token; pinned virtual IP).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerConfig {
    pub name: String,
    pub token: String,
    pub ip: Ipv4Addr,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Allow global-proxy egress to the internet through this server.
    #[serde(default = "yes")]
    pub allow_global: bool,
    /// Allow HTTP-proxy stream dialing.
    #[serde(default = "yes")]
    pub allow_http_proxy: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub listeners: Vec<ListenerConfig>,
    pub network: NetworkConfig,
    pub security: SecurityConfig,
    pub web: WebConfig,
    #[serde(default)]
    pub peers: Vec<PeerConfig>,
}

impl ServerConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)?;
        toml::from_str(&text).map_err(|e| Error::Config(format!("parse {}: {e}", path.display())))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let text =
            toml::to_string_pretty(self).map_err(|e| Error::Config(format!("serialize: {e}")))?;
        std::fs::write(path, text)?;
        Ok(())
    }

    /// Prefix length of the virtual subnet (e.g. 24).
    pub fn prefix_len(&self) -> Result<u8> {
        let net: ipnet::Ipv4Net = self
            .network
            .subnet
            .parse()
            .map_err(|e| Error::Config(format!("bad subnet {}: {e}", self.network.subnet)))?;
        Ok(net.prefix_len())
    }

    /// The configured IPv6 ULA subnet, if any (e.g. `fd66::/64`).
    pub fn subnet6_net(&self) -> Option<ipnet::Ipv6Net> {
        self.network.subnet6.as_ref().and_then(|s| s.parse().ok())
    }

    pub fn find_peer(&self, token: &str) -> Option<&PeerConfig> {
        self.peers.iter().find(|p| p.token == token)
    }

    /// A starter config with one TCP listener and one example peer.
    pub fn template() -> Self {
        ServerConfig {
            listeners: vec![ListenerConfig {
                transport: TransportKind::Tcp,
                bind: "0.0.0.0:8443".into(),
                tls: true,
            }],
            network: NetworkConfig {
                subnet: "10.66.0.0/24".into(),
                gateway: Ipv4Addr::new(10, 66, 0, 1),
                mtu: 1380,
                dns: vec![Ipv4Addr::new(8, 8, 8, 8), Ipv4Addr::new(1, 1, 1, 1)],
                tun_name: "et0".into(),
                egress_iface: None,
                subnet6: default_subnet6(),
                gateway6: default_gateway6(),
                dns6: default_dns6(),
            },
            security: SecurityConfig {
                noise_psk: generate_psk(),
                tls_cert_path: None,
                tls_key_path: None,
            },
            web: WebConfig {
                bind: "127.0.0.1:9000".into(),
                admin_token: generate_token(),
            },
            peers: vec![PeerConfig {
                name: "example".into(),
                token: generate_token(),
                ip: Ipv4Addr::new(10, 66, 0, 2),
                enabled: true,
                allow_global: true,
                allow_http_proxy: true,
            }],
        }
    }
}
