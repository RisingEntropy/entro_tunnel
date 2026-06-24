//! The persisted client profile (`client.toml`), shared by CLI and GUI.

use entrotunnel_core::config::{SessionMode, TransportKind};
use entrotunnel_core::{Error, Result};
use serde::{Deserialize, Serialize};
use std::net::Ipv4Addr;
use std::path::Path;

fn default_tun_name() -> String {
    "et0".to_string()
}
fn default_http_listen() -> String {
    "127.0.0.1:7890".to_string()
}
fn default_transport() -> TransportKind {
    TransportKind::Tcp
}
fn is_zero(n: &u16) -> bool {
    *n == 0
}
fn is_false(b: &bool) -> bool {
    !*b
}
/// Serde default for opt-in-by-default booleans (e.g. the IPv6 kill-switch).
fn default_true() -> bool {
    true
}
fn is_tcp(t: &TransportKind) -> bool {
    matches!(t, TransportKind::Tcp)
}

/// One server endpoint the client can connect to. A profile may list several;
/// `ClientConfig::selected_server` picks the active one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerEntry {
    /// Unique label used to select this server.
    pub name: String,
    pub host: String,
    pub port: u16,
    #[serde(default = "default_transport")]
    pub transport: TransportKind,
    /// Pre-shared identity token (matches a server peer record).
    pub token: String,
    /// base64 32-byte Noise PSK (raw-TCP channel auth).
    pub noise_psk: String,
    /// TLS transports: skip certificate verification (self-hosted; logged).
    #[serde(default, skip_serializing_if = "is_false")]
    pub tls_skip_verify: bool,
    /// TLS SNI / server name override (defaults to `host`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_name: Option<String>,
}

impl ServerEntry {
    pub fn sni(&self) -> String {
        self.server_name.clone().unwrap_or_else(|| self.host.clone())
    }
}

/// One client profile. The Tauri app stores a list of these and activates one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientConfig {
    /// Human-friendly profile name.
    #[serde(default)]
    pub name: String,

    /// Name of the active server in `servers`; falls back to the first entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_server: Option<String>,

    // --- connection-level settings (shared across servers) ---
    pub mode: SessionMode,

    /// Client-requested virtual IP (server may override / pin).
    #[serde(default)]
    pub requested_ip: Option<Ipv4Addr>,

    /// Shown in the admin panel.
    #[serde(default)]
    pub client_name: Option<String>,

    /// TUN device name (global-proxy / VPN modes).
    #[serde(default = "default_tun_name")]
    pub tun_name: String,

    /// Local listen address for HTTP-proxy mode.
    #[serde(default = "default_http_listen")]
    pub http_listen: String,

    /// Also join the server's VPN (peer LAN) regardless of `mode`. In a proxy
    /// mode this additionally brings up a TUN routed at just the virtual subnet,
    /// so the client can reach other peers by IP while internet still follows the
    /// proxy. Implied for [`SessionMode::Vpn`]. Needs admin/root (creates a TUN).
    #[serde(default, skip_serializing_if = "is_false")]
    pub join_vpn: bool,

    // --- legacy single-server fields (used only when `servers` is empty) ---
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub server_host: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub server_port: u16,
    #[serde(default = "default_transport", skip_serializing_if = "is_tcp")]
    pub transport: TransportKind,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub token: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub noise_psk: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub tls_skip_verify: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_name: Option<String>,

    // --- arrays of tables (serialized last, per TOML rules) ---
    /// The configured servers; pick the active one with `selected_server`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub servers: Vec<ServerEntry>,

    /// Split-tunnel routing rules: send specific destinations out a chosen NIC
    /// instead of the virtual tunnel. Evaluated as more-specific routes, so they
    /// win over the tunnel's catch-all. Empty = everything follows the mode.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub routes: Vec<RouteRule>,

    /// Whitelist vs blacklist for the global-proxy catch-all (see [`SplitMode`]).
    #[serde(default)]
    pub split_mode: SplitMode,

    /// Proxy chain: ordered server names to relay through (empty = single hop).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chain: Vec<String>,

    /// IPv6 kill-switch (see [`ConnectionSettings::ipv6_killswitch`]).
    #[serde(default = "default_true")]
    pub ipv6_killswitch: bool,
}

/// One split-tunnel rule: "this destination goes via that interface".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteRule {
    /// A domain (`example.com`, resolved to IPs at connect time) or an
    /// IP / CIDR (`1.2.3.4`, `10.0.0.0/8`). IPv4 only.
    pub target: String,
    /// Where matching traffic goes:
    /// - `"tunnel"` — force through the virtual NIC,
    /// - `"direct"` — bypass the tunnel via the host's original default NIC,
    /// - any other value — a NIC name to send it out (e.g. `"eth1"`, `"en0"`).
    pub via: String,
    /// Explicit gateway when `via` is a NIC name and the destination is off-link.
    #[serde(default)]
    pub gateway: Option<Ipv4Addr>,
}

/// How the global-proxy catch-all interacts with the split-tunnel rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SplitMode {
    /// Tunnel EVERYTHING; the rules carve out exceptions that bypass the tunnel
    /// (typically `via direct`). The classic global-proxy behavior. Default.
    #[default]
    Blacklist,
    /// Tunnel NOTHING by default; only the listed rules (typically `via tunnel`)
    /// go through the tunnel — everything else stays on the normal direct route.
    Whitelist,
}

impl ClientConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)?;
        toml::from_str(&text).map_err(|e| Error::Config(format!("parse {}: {e}", path.display())))
    }

    /// Serialize the full config to a pretty TOML string (the CLI's `client.toml`
    /// format). Used by the GUI's "export profile as TOML" too.
    pub fn to_toml(&self) -> Result<String> {
        toml::to_string_pretty(self).map_err(|e| Error::Config(format!("serialize: {e}")))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        std::fs::write(path, self.to_toml()?)?;
        Ok(())
    }

    /// Resolve the active server: the one named by `selected_server`, else the
    /// first entry in `servers`, else the legacy flat single-server fields.
    ///
    /// A `selected_server` that no longer matches any entry (e.g. the server was
    /// renamed) falls back to the first server rather than failing — the GUI
    /// already shows the first server in that case, so connecting should agree.
    pub fn active_server(&self) -> Result<ServerEntry> {
        if !self.servers.is_empty() {
            let entry = match &self.selected_server {
                Some(name) => self.servers.iter().find(|s| &s.name == name).unwrap_or_else(|| {
                    tracing::warn!("selected_server '{name}' not found; using the first server");
                    &self.servers[0]
                }),
                None => &self.servers[0],
            };
            return Ok(entry.clone());
        }
        if self.server_host.is_empty() {
            return Err(Error::Config("no servers configured".into()));
        }
        Ok(ServerEntry {
            name: "default".into(),
            host: self.server_host.clone(),
            port: self.server_port,
            transport: self.transport,
            token: self.token.clone(),
            noise_psk: self.noise_psk.clone(),
            tls_skip_verify: self.tls_skip_verify,
            server_name: self.server_name.clone(),
        })
    }

    /// Names of all configured servers (legacy config reports `["default"]`).
    pub fn server_names(&self) -> Vec<String> {
        if self.servers.is_empty() {
            vec!["default".to_string()]
        } else {
            self.servers.iter().map(|s| s.name.clone()).collect()
        }
    }

    /// Compose the engine's runtime config from a server-only [`Profile`] and the
    /// locally-chosen [`ConnectionSettings`] (mode, TUN/HTTP, routes). This is how
    /// the GUI builds what the engine consumes: the profile carries *only* server
    /// connection details, everything else comes from the local connection.
    pub fn compose(profile: &Profile, s: &ConnectionSettings) -> Self {
        ClientConfig {
            name: profile.name.clone(),
            selected_server: profile.selected_server.clone(),
            mode: s.mode,
            requested_ip: s.requested_ip,
            client_name: s.client_name.clone(),
            tun_name: s.tun_name.clone(),
            http_listen: s.http_listen.clone(),
            join_vpn: s.join_vpn,
            // legacy flat fields stay empty: composed configs always use `servers`.
            server_host: String::new(),
            server_port: 0,
            transport: default_transport(),
            token: String::new(),
            noise_psk: String::new(),
            tls_skip_verify: false,
            server_name: None,
            servers: profile.servers.clone(),
            routes: s.routes.clone(),
            split_mode: s.split_mode,
            chain: s.chain.clone(),
            ipv6_killswitch: s.ipv6_killswitch,
        }
    }
}

/// A portable, **server-only** client profile: the unit the server exports and
/// the client imports / stores. It deliberately carries no mode or TUN settings
/// — those are chosen locally at connect time (see [`ConnectionSettings`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub name: String,
    /// Active server by name; falls back to the first entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_server: Option<String>,
    #[serde(default)]
    pub servers: Vec<ServerEntry>,
}

impl Profile {
    /// Resolve the active server (named by `selected_server`, else the first).
    /// A stale `selected_server` (e.g. the server was renamed) falls back to the
    /// first server instead of failing.
    pub fn active_server(&self) -> Result<ServerEntry> {
        if self.servers.is_empty() {
            return Err(Error::Config("profile has no servers".into()));
        }
        let entry = match &self.selected_server {
            Some(name) => self.servers.iter().find(|s| &s.name == name).unwrap_or_else(|| {
                tracing::warn!("selected_server '{name}' not found; using the first server");
                &self.servers[0]
            }),
            None => &self.servers[0],
        };
        Ok(entry.clone())
    }

    pub fn server_names(&self) -> Vec<String> {
        self.servers.iter().map(|s| s.name.clone()).collect()
    }

    /// Encode as a single portable line: `entro://<base64-json>`. This is the
    /// string the web admin shows for copy-paste and the client imports.
    pub fn encode_link(&self) -> String {
        use base64::Engine;
        let json = serde_json::to_vec(self).unwrap_or_default();
        let b64 = base64::engine::general_purpose::STANDARD.encode(json);
        format!("{LINK_SCHEME}{b64}")
    }

    /// Decode a `entro://<base64-json>` link back into a profile. Tolerant of a
    /// missing scheme prefix and of standard- vs url-safe base64.
    pub fn decode_link(s: &str) -> Result<Profile> {
        use base64::Engine;
        let body = s.trim();
        let body = body.strip_prefix(LINK_SCHEME).unwrap_or(body).trim();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(body)
            .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(body))
            .or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(body))
            .map_err(|e| Error::Config(format!("invalid config link (base64): {e}")))?;
        serde_json::from_slice(&bytes)
            .map_err(|e| Error::Config(format!("invalid config link (json): {e}")))
    }
}

/// URL-ish scheme for the portable [`Profile`] link.
pub const LINK_SCHEME: &str = "entro://";

/// Connection-level settings chosen on the client (the "local connection"):
/// which mode to run and the parameters for it. Stored once per device, applied
/// to whatever profile/server is active.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionSettings {
    pub mode: SessionMode,
    #[serde(default)]
    pub requested_ip: Option<Ipv4Addr>,
    #[serde(default)]
    pub client_name: Option<String>,
    #[serde(default = "default_tun_name")]
    pub tun_name: String,
    #[serde(default = "default_http_listen")]
    pub http_listen: String,
    /// Also join the server's VPN peer LAN (see [`ClientConfig::join_vpn`]).
    #[serde(default)]
    pub join_vpn: bool,
    #[serde(default)]
    pub routes: Vec<RouteRule>,
    /// Whitelist vs blacklist for the global-proxy catch-all (see [`SplitMode`]).
    #[serde(default)]
    pub split_mode: SplitMode,
    /// Proxy chain: ordered server names to relay through (empty = single hop).
    #[serde(default)]
    pub chain: Vec<String>,
    /// IPv6 kill-switch: in global-proxy (full-tunnel) mode, when the server is
    /// v4-only, block the host's native IPv6 so it can't bypass the tunnel and
    /// leak the real IP/location. On by default; turn off to keep native IPv6.
    #[serde(default = "default_true")]
    pub ipv6_killswitch: bool,
}

impl Default for ConnectionSettings {
    fn default() -> Self {
        ConnectionSettings {
            mode: SessionMode::GlobalProxy,
            requested_ip: None,
            client_name: None,
            tun_name: default_tun_name(),
            http_listen: default_http_listen(),
            join_vpn: false,
            routes: Vec::new(),
            split_mode: SplitMode::default(),
            chain: Vec::new(),
            ipv6_killswitch: true,
        }
    }
}

/// A bundle of profiles for one-file export/import (TOML). Serializes as a
/// sequence of `[[profiles]]` tables (each with its own `[[profiles.servers]]`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProfileBundle {
    #[serde(default)]
    pub profiles: Vec<Profile>,
}

impl ProfileBundle {
    pub fn to_toml(&self) -> Result<String> {
        toml::to_string_pretty(self).map_err(|e| Error::Config(format!("serialize: {e}")))
    }
    pub fn from_toml(text: &str) -> Result<Self> {
        toml::from_str(text).map_err(|e| Error::Config(format!("parse profiles TOML: {e}")))
    }
}

/// Parse a TOML import that may be EITHER a multi-profile bundle (`[[profiles]]`,
/// from "Export all") OR a single `client.toml` (a full [`ClientConfig`], from
/// "Export TOML"). Returns the server-only profile(s) found.
pub fn profiles_from_toml(text: &str) -> Result<Vec<Profile>> {
    // Multi-profile bundle (only if it actually has entries — a client.toml also
    // parses as a ProfileBundle with an empty `profiles`, since unknown top-level
    // keys are ignored).
    if let Ok(b) = toml::from_str::<ProfileBundle>(text) {
        if !b.profiles.is_empty() {
            return Ok(b.profiles);
        }
    }
    // Single client.toml → its server-only profile (mode/routes are device-local
    // and intentionally dropped).
    let cfg: ClientConfig = toml::from_str(text)
        .map_err(|e| Error::Config(format!("not a profile bundle or client.toml: {e}")))?;
    if cfg.servers.is_empty() {
        return Err(Error::Config(
            "TOML has no profiles and no [[servers]] to import".into(),
        ));
    }
    Ok(vec![Profile {
        name: if cfg.name.is_empty() { "imported".into() } else { cfg.name },
        selected_server: cfg.selected_server,
        servers: cfg.servers,
    }])
}

#[cfg(test)]
mod export_tests {
    use super::*;

    fn srv(name: &str, host: &str, port: u16, tls_skip: bool) -> ServerEntry {
        ServerEntry {
            name: name.into(),
            host: host.into(),
            port,
            transport: default_transport(),
            token: format!("TOKEN-{name}"),
            noise_psk: format!("PSK-{name}"),
            tls_skip_verify: tls_skip,
            server_name: None,
        }
    }

    /// The TOML export must contain the WHOLE profile — every server, not just the
    /// selected one — plus the connection settings.
    #[test]
    fn export_includes_all_servers_and_settings() {
        let profile = Profile {
            name: "home-lab".into(),
            selected_server: Some("aliyun".into()),
            servers: vec![
                srv("hkg", "141.11.149.77", 8443, true),
                srv("new", "14.137.244.49", 8443, true),
                srv("aliyun", "tun1.hydeng.cn", 443, false),
            ],
        };
        let settings = ConnectionSettings {
            mode: SessionMode::GlobalProxy,
            split_mode: SplitMode::Whitelist,
            routes: vec![RouteRule {
                target: "example.com".into(),
                via: "tunnel".into(),
                gateway: None,
            }],
            ..Default::default()
        };
        let toml = ClientConfig::compose(&profile, &settings).to_toml().unwrap();
        println!("\n---REAL TOML EXPORT (3-server profile)---\n{toml}\n---END---\n");

        // All three servers present (not just the selected one).
        assert_eq!(toml.matches("[[servers]]").count(), 3);
        for s in ["hkg", "new", "aliyun"] {
            assert!(toml.contains(&format!("name = \"{s}\"")), "missing server {s}");
        }
        // Connection settings carried too.
        assert!(toml.contains("split_mode = \"whitelist\""));
        assert!(toml.contains("selected_server = \"aliyun\""));
    }

    /// Export ALL profiles to one TOML and re-import them losslessly.
    #[test]
    fn profile_bundle_roundtrips_all() {
        let bundle = ProfileBundle {
            profiles: vec![
                Profile {
                    name: "home".into(),
                    selected_server: Some("a".into()),
                    servers: vec![srv("a", "1.1.1.1", 8443, false), srv("b", "2.2.2.2", 443, true)],
                },
                Profile {
                    name: "work".into(),
                    selected_server: None,
                    servers: vec![srv("c", "3.3.3.3", 8443, false)],
                },
            ],
        };
        let toml = bundle.to_toml().unwrap();
        println!("\n---PROFILE BUNDLE TOML---\n{toml}\n---END---\n");
        let back = ProfileBundle::from_toml(&toml).unwrap();
        assert_eq!(back.profiles.len(), 2);
        assert_eq!(back.profiles[0].name, "home");
        assert_eq!(back.profiles[0].servers.len(), 2);
        assert_eq!(back.profiles[1].name, "work");
        assert_eq!(back.profiles[1].servers[0].host, "3.3.3.3");
    }

    /// `profiles_from_toml` accepts BOTH a bundle and a single client.toml.
    #[test]
    fn import_accepts_bundle_and_single_client_toml() {
        // (a) A multi-profile bundle.
        let bundle = ProfileBundle {
            profiles: vec![Profile {
                name: "p1".into(),
                selected_server: None,
                servers: vec![srv("a", "1.1.1.1", 8443, false)],
            }],
        }
        .to_toml()
        .unwrap();
        let got = profiles_from_toml(&bundle).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "p1");

        // (b) A single client.toml (what "Export TOML" produces) → its profile.
        let profile = Profile {
            name: "home-lab".into(),
            selected_server: Some("hk".into()),
            servers: vec![srv("hk", "9.9.9.9", 443, true), srv("hk2", "8.8.8.8", 8443, false)],
        };
        let settings = ConnectionSettings { split_mode: SplitMode::Whitelist, ..Default::default() };
        let client_toml = ClientConfig::compose(&profile, &settings).to_toml().unwrap();
        let got = profiles_from_toml(&client_toml).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "home-lab");
        assert_eq!(got[0].servers.len(), 2);
        assert_eq!(got[0].selected_server.as_deref(), Some("hk"));
    }
}
