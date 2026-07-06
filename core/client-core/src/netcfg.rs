//! OS network configuration (addresses, routes, DNS) for the client side.
//!
//! * **Linux** — `iproute2` (`ip`). Robust and dependency-free (matters for the
//!   Dockerized CLI test).
//! * **macOS** — `ifconfig` + `route` (+ best-effort `/etc/resolv.conf`).
//! * **Windows** — `netsh` + `route` (assigns the Wintun adapter's IP, installs
//!   the split-default capture, sets DNS on the adapter).
//!
//! [`apply`] returns a [`NetGuard`]; dropping it restores the previous routing
//! and DNS state (best-effort, synchronously).

use crate::config::ClientConfig;
use entrotunnel_core::protocol::Welcome;
use entrotunnel_core::{Error, Result};
use std::net::IpAddr;

/// Restores network state when dropped.
pub struct NetGuard {
    /// `(command, args)` pairs to run on cleanup, in order.
    undo: Vec<(String, Vec<String>)>,
    /// Original `/etc/resolv.conf` contents to restore.
    resolv_backup: Option<String>,
}

impl Drop for NetGuard {
    fn drop(&mut self) {
        for (cmd, args) in &self.undo {
            let _ = std::process::Command::new(cmd).args(args).status();
        }
        if let Some(backup) = self.resolv_backup.take() {
            if write_in_place("/etc/resolv.conf", &backup).is_ok() {
                tracing::debug!("restored /etc/resolv.conf");
            }
        }
    }
}

fn write_in_place(path: &str, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    // Truncate-in-place so a bind-mounted resolv.conf (Docker) survives.
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)?;
    f.write_all(contents.as_bytes())
}

// ----------------------------------------------------------------------------
// Linux
// ----------------------------------------------------------------------------
#[cfg(target_os = "linux")]
async fn run_ip(args: &[&str]) -> Result<()> {
    let out = tokio::process::Command::new("ip")
        .args(args)
        .output()
        .await
        .map_err(|e| Error::Transport(format!("spawn `ip {}`: {e}", args.join(" "))))?;
    if !out.status.success() {
        return Err(Error::Transport(format!(
            "`ip {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
async fn try_ip(args: &[&str]) {
    if let Err(e) = run_ip(args).await {
        tracing::debug!("{e}");
    }
}

#[cfg(target_os = "linux")]
async fn default_route() -> Result<(String, String)> {
    let out = tokio::process::Command::new("ip")
        .args(["route", "show", "default"])
        .output()
        .await
        .map_err(|e| Error::Transport(format!("ip route: {e}")))?;
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().next().unwrap_or_default();
    let toks: Vec<&str> = line.split_whitespace().collect();
    let gw = toks
        .iter()
        .position(|t| *t == "via")
        .and_then(|i| toks.get(i + 1))
        .map(|s| s.to_string());
    let dev = toks
        .iter()
        .position(|t| *t == "dev")
        .and_then(|i| toks.get(i + 1))
        .map(|s| s.to_string());
    match (gw, dev) {
        (Some(g), Some(d)) => Ok((g, d)),
        _ => Err(Error::Transport(format!("cannot parse default route: {line:?}"))),
    }
}

/// The IPv6 default route's `(gateway, dev)`, if the host has v6 connectivity.
/// Only needed to pin a v6 *carrier* so it doesn't loop into the tunnel.
#[cfg(target_os = "linux")]
async fn default_route6() -> Option<(String, String)> {
    let out = tokio::process::Command::new("ip")
        .args(["-6", "route", "show", "default"])
        .output()
        .await
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let toks: Vec<&str> = text.lines().next().unwrap_or_default().split_whitespace().collect();
    let gw = toks.iter().position(|t| *t == "via").and_then(|i| toks.get(i + 1)).map(|s| s.to_string());
    let dev = toks.iter().position(|t| *t == "dev").and_then(|i| toks.get(i + 1)).map(|s| s.to_string());
    match (gw, dev) {
        (Some(g), Some(d)) => Some((g, d)),
        _ => None,
    }
}

#[cfg(target_os = "linux")]
pub async fn apply(
    cfg: &ClientConfig,
    welcome: &Welcome,
    ifname: &str,
    server_ip: IpAddr,
) -> Result<NetGuard> {
    use entrotunnel_core::config::SessionMode;

    let mut undo: Vec<(String, Vec<String>)> = Vec::new();
    let mut resolv_backup = None;

    let cidr = format!("{}/{}", welcome.assigned_ip, welcome.prefix_len);
    run_ip(&["addr", "add", &cidr, "dev", ifname]).await?;
    run_ip(&["link", "set", "dev", ifname, "mtu", &welcome.mtu.to_string()]).await?;
    run_ip(&["link", "set", "dev", ifname, "up"]).await?;

    // IPv6 address (NAT66) — present only when the server offers v6. The TUN
    // device drop removes the address on disconnect (same as the v4 address).
    if let Some(ip6) = welcome.assigned_ip6 {
        let cidr6 = format!("{ip6}/{}", welcome.prefix6.max(1));
        try_ip(&["-6", "addr", "add", &cidr6, "dev", ifname]).await;
    }

    let orig = default_route().await.ok();

    // Split-tunnel rules first: resolve any domain targets while the original
    // routing/DNS is still in effect, then install them as more-specific routes
    // (so they win over the tunnel's catch-all regardless of mode).
    apply_route_rules(cfg, ifname, &orig, server_ip, &mut undo).await;

    match cfg.mode {
        SessionMode::GlobalProxy => {
            // Always pin the server's own address to the physical gateway so the
            // tunnel's carrier traffic never loops, regardless of split mode.
            if let Some((gw, dev)) = &orig {
                let host = format!("{server_ip}/32");
                try_ip(&["route", "add", &host, "via", gw, "dev", dev]).await;
                undo.push(("ip".into(), vec!["route".into(), "del".into(), host]));
            } else {
                tracing::warn!("no default route; server pin route skipped");
            }

            if cfg.split_mode == crate::config::SplitMode::Whitelist {
                // Whitelist: capture nothing here — only the per-rule `via tunnel`
                // routes (installed above) go through the tunnel; everything else
                // stays on the host's normal route, and system DNS is left alone.
                tracing::info!("split mode = whitelist: only listed routes use {ifname}");
            } else {
                // Blacklist (default): capture ALL traffic + send DNS through the
                // tunnel; the per-rule routes carve out direct exceptions.
                run_ip(&["route", "add", "0.0.0.0/1", "dev", ifname]).await?;
                undo.push(("ip".into(), vec!["route".into(), "del".into(), "0.0.0.0/1".into()]));
                run_ip(&["route", "add", "128.0.0.0/1", "dev", ifname]).await?;
                undo.push(("ip".into(), vec!["route".into(), "del".into(), "128.0.0.0/1".into()]));

                // IPv6 default capture (::/1 + 8000::/1, mirroring the v4 split) so
                // the host's own v6 default still exists underneath ours.
                if welcome.assigned_ip6.is_some() {
                    // If the tunnel's *carrier* is itself v6, pin the server's real
                    // v6 to the physical gateway so it doesn't loop into the tunnel.
                    if let IpAddr::V6(s6) = server_ip {
                        if let Some((gw6, dev6)) = default_route6().await {
                            let host = format!("{s6}/128");
                            try_ip(&["-6", "route", "add", &host, "via", &gw6, "dev", &dev6]).await;
                            undo.push(("ip".into(), vec!["-6".into(), "route".into(), "del".into(), host]));
                        }
                    }
                    try_ip(&["-6", "route", "add", "::/1", "dev", ifname]).await;
                    undo.push(("ip".into(), vec!["-6".into(), "route".into(), "del".into(), "::/1".into()]));
                    try_ip(&["-6", "route", "add", "8000::/1", "dev", ifname]).await;
                    undo.push(("ip".into(), vec!["-6".into(), "route".into(), "del".into(), "8000::/1".into()]));
                } else if cfg.ipv6_killswitch {
                    // IPv6 leak protection (kill-switch): the server is v4-only, so
                    // the host's NATIVE IPv6 (e.g. a China-Telecom 240e:: address)
                    // would otherwise bypass the tunnel and expose your real IP /
                    // location. Install `unreachable` routes for all global v6 so
                    // apps fail fast and fall back to v4 (which IS tunneled).
                    // Link-local (fe80::/10) and on-link prefixes are more specific,
                    // so NDP/SLAAC keep working.
                    try_ip(&["-6", "route", "add", "unreachable", "::/1"]).await;
                    undo.push(("ip".into(), vec!["-6".into(), "route".into(), "del".into(), "::/1".into()]));
                    try_ip(&["-6", "route", "add", "unreachable", "8000::/1"]).await;
                    undo.push(("ip".into(), vec!["-6".into(), "route".into(), "del".into(), "8000::/1".into()]));
                    tracing::info!("IPv6 leak protection: server is v4-only — native IPv6 blocked while connected");
                } else {
                    tracing::warn!("IPv6 kill-switch OFF: server is v4-only, so native IPv6 will bypass the tunnel and may leak your real IP/location");
                }

                if !welcome.dns.is_empty() || !welcome.dns6.is_empty() {
                    if let Ok(prev) = std::fs::read_to_string("/etc/resolv.conf") {
                        resolv_backup = Some(prev);
                    }
                    // v4 + v6 resolvers; both ride the tunnel via the routes above.
                    let body: String = welcome
                        .dns
                        .iter()
                        .map(|ip| format!("nameserver {ip}\n"))
                        .chain(welcome.dns6.iter().map(|ip| format!("nameserver {ip}\n")))
                        .collect();
                    if let Err(e) = write_in_place("/etc/resolv.conf", &body) {
                        tracing::warn!("could not set /etc/resolv.conf: {e}");
                    } else {
                        tracing::info!("DNS → v4 {:?} v6 {:?}", welcome.dns, welcome.dns6);
                    }
                }
            }
        }
        SessionMode::Vpn => {
            tracing::info!("VPN mode: virtual subnet reachable via {ifname}");
        }
        // No OS routing for proxy modes (the OS-proxy switch is handled by the
        // engine's sysproxy guard, not here).
        SessionMode::HttpProxy | SessionMode::SystemProxy => {}
    }

    Ok(NetGuard { undo, resolv_backup })
}

/// Bring up the TUN and route *only* the virtual subnet through it — used when a
/// proxy-mode client also joins the VPN peer LAN. No default route / DNS change,
/// so internet traffic keeps following the proxy mode. (Linux: `ip addr add`
/// auto-creates the on-link subnet route.)
#[cfg(target_os = "linux")]
pub async fn apply_vpn_lan(welcome: &Welcome, ifname: &str) -> Result<NetGuard> {
    let mut undo: Vec<(String, Vec<String>)> = Vec::new();
    let cidr = format!("{}/{}", welcome.assigned_ip, welcome.prefix_len);
    run_ip(&["addr", "add", &cidr, "dev", ifname]).await?;
    run_ip(&["link", "set", "dev", ifname, "mtu", &welcome.mtu.to_string()]).await?;
    run_ip(&["link", "set", "dev", ifname, "up"]).await?;
    undo.push((
        "ip".into(),
        vec!["addr".into(), "del".into(), cidr, "dev".into(), ifname.to_string()],
    ));
    tracing::info!("VPN LAN: virtual subnet reachable via {ifname} (proxy mode + join)");
    Ok(NetGuard { undo, resolv_backup: None })
}

/// Resolve a rule target to one or more IPv4 CIDR strings. Domains are resolved
/// via the system resolver (IPv4 answers only); IPv6 targets are skipped.
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
async fn resolve_rule_targets(target: &str) -> Vec<String> {
    use std::net::{Ipv4Addr, IpAddr};
    if let Ok(net) = target.parse::<ipnet::Ipv4Net>() {
        return vec![net.to_string()];
    }
    if let Ok(ip) = target.parse::<Ipv4Addr>() {
        return vec![format!("{ip}/32")];
    }
    if target.parse::<IpAddr>().is_ok() || target.parse::<ipnet::IpNet>().is_ok() {
        tracing::warn!("route rule target {target} is IPv6; skipped (IPv4-only routing)");
        return vec![];
    }
    // Treat as a domain.
    match tokio::net::lookup_host((target, 0u16)).await {
        Ok(addrs) => {
            let mut v: Vec<String> = addrs
                .filter_map(|sa| match sa.ip() {
                    IpAddr::V4(ip) => Some(format!("{ip}/32")),
                    IpAddr::V6(_) => None,
                })
                .collect();
            v.sort();
            v.dedup();
            if v.is_empty() {
                tracing::warn!("route rule target {target} resolved to no IPv4 addresses");
            }
            v
        }
        Err(e) => {
            tracing::warn!("route rule: cannot resolve {target}: {e}");
            vec![]
        }
    }
}

/// Install the per-destination split-tunnel routes (Linux).
#[cfg(target_os = "linux")]
async fn apply_route_rules(
    cfg: &ClientConfig,
    tun_ifname: &str,
    orig: &Option<(String, String)>,
    server_ip: IpAddr,
    undo: &mut Vec<(String, Vec<String>)>,
) {
    let server_route = format!("{server_ip}/32");
    let mut seen = std::collections::HashSet::new();
    for rule in &cfg.routes {
        for route in resolve_rule_targets(&rule.target).await {
            // Never reroute the server's own address — that would send the
            // tunnel's own carrier traffic into the tunnel and deadlock.
            if route == server_route {
                tracing::warn!(
                    "route rule {} resolves to the server IP {server_ip}; skipped to keep the link up",
                    rule.target
                );
                continue;
            }
            if !seen.insert(route.clone()) {
                tracing::warn!("route {route} already claimed by an earlier rule; later rule ignored");
                continue;
            }

            // Build the add args; the undo is the same vector with add→del so
            // teardown deletes exactly what we installed (not a colliding route).
            let mut add: Vec<String> = vec!["route".into(), "add".into(), route.clone()];
            match rule.via.as_str() {
                "tunnel" => add.extend(["dev".into(), tun_ifname.to_string()]),
                "direct" => match orig {
                    Some((gw, dev)) => {
                        add.extend(["via".into(), gw.clone(), "dev".into(), dev.clone()]);
                    }
                    None => {
                        tracing::warn!("route rule {} via direct: no default route", rule.target);
                        continue;
                    }
                },
                iface => {
                    if let Some(gw) = &rule.gateway {
                        add.extend(["via".into(), gw.to_string()]);
                    }
                    add.extend(["dev".into(), iface.to_string()]);
                }
            }
            let argref: Vec<&str> = add.iter().map(String::as_str).collect();
            match run_ip(&argref).await {
                Ok(()) => {
                    tracing::info!(target = %rule.target, route = %route, via = %rule.via, "split-route applied");
                    let mut del = add.clone();
                    del[1] = "del".into();
                    undo.push(("ip".into(), del));
                }
                Err(e) => tracing::warn!("route rule {route} via {}: {e}", rule.via),
            }
        }
    }
}

// ----------------------------------------------------------------------------
// macOS (utun) — compiled but runtime-untested in CI; needs `sudo`.
// ----------------------------------------------------------------------------
#[cfg(target_os = "macos")]
async fn run(cmd: &str, args: &[&str]) -> Result<()> {
    let out = tokio::process::Command::new(cmd)
        .args(args)
        .output()
        .await
        .map_err(|e| Error::Transport(format!("spawn `{cmd}`: {e}")))?;
    if !out.status.success() {
        return Err(Error::Transport(format!(
            "`{cmd} {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
async fn default_gateway() -> Result<String> {
    let out = tokio::process::Command::new("route")
        .args(["-n", "get", "default"])
        .output()
        .await
        .map_err(|e| Error::Transport(format!("route get default: {e}")))?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .find_map(|l| l.trim().strip_prefix("gateway:"))
        .map(|s| s.trim().to_string())
        .ok_or_else(|| Error::Transport("no default gateway".into()))
}

/// Run a command and return its stdout (None on spawn/exit failure).
#[cfg(target_os = "macos")]
async fn cmd_out(cmd: &str, args: &[&str]) -> Option<String> {
    let out = tokio::process::Command::new(cmd).args(args).output().await.ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The IPv6 default gateway, if the host has v6 connectivity.
#[cfg(target_os = "macos")]
async fn default_gateway6() -> Option<String> {
    let text = cmd_out("route", &["-n", "get", "-inet6", "default"]).await?;
    text.lines()
        .find_map(|l| l.trim().strip_prefix("gateway:"))
        .map(|s| s.trim().to_string())
}

/// The `networksetup` service name (e.g. "Wi-Fi") for the interface carrying the
/// default route. macOS GUI apps resolve via SystemConfiguration, not
/// /etc/resolv.conf, so DNS must be set per-service to actually take effect.
#[cfg(target_os = "macos")]
async fn primary_service() -> Option<String> {
    let def = cmd_out("route", &["-n", "get", "default"]).await?;
    let dev = def
        .lines()
        .find_map(|l| l.trim().strip_prefix("interface:"))
        .map(|s| s.trim().to_string())?;
    let order = cmd_out("networksetup", &["-listnetworkserviceorder"]).await?;
    // Blocks pair "(1) Wi-Fi" with "(Hardware Port: Wi-Fi, Device: en0)".
    let mut name: Option<String> = None;
    for line in order.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix('(') {
            if rest.starts_with("Hardware Port:") {
                if t.contains(&format!("Device: {dev})")) {
                    return name;
                }
            } else if let Some(i) = rest.find(") ") {
                name = Some(rest[i + 2..].trim().to_string());
            }
        }
    }
    None
}

#[cfg(target_os = "macos")]
pub async fn apply(
    cfg: &ClientConfig,
    welcome: &Welcome,
    ifname: &str,
    server_ip: IpAddr,
) -> Result<NetGuard> {
    use entrotunnel_core::config::SessionMode;

    let mut undo: Vec<(String, Vec<String>)> = Vec::new();
    let mut resolv_backup = None;

    let ip = welcome.assigned_ip.to_string();
    let gw = welcome.gateway.to_string();
    let mtu = welcome.mtu.to_string();
    // utun is point-to-point: local = assigned IP, peer = gateway.
    run("ifconfig", &[ifname, "inet", &ip, &gw, "up"]).await?;
    let _ = run("ifconfig", &[ifname, "mtu", &mtu]).await;

    // IPv6 address on the utun (NAT66), when the server offers v6.
    if let Some(ip6) = welcome.assigned_ip6 {
        let plen = welcome.prefix6.max(1).to_string();
        let _ = run("ifconfig", &[ifname, "inet6", &ip6.to_string(), "prefixlen", &plen]).await;
    }

    let orig_gw = default_gateway().await.ok();
    apply_route_rules(cfg, ifname, &gw, &orig_gw, server_ip, &mut undo).await;

    // Capture the physical network service NOW — before the tunnel's
    // default-capture routes are added (after which `route get default` resolves
    // to the utun and the service can't be found). Reused for DNS + v6 kill-switch.
    let phys_service = primary_service().await;

    let subnet = ipnet::Ipv4Net::new(welcome.assigned_ip, welcome.prefix_len)
        .map(|n| n.network())
        .map(|net| format!("{net}/{}", welcome.prefix_len))
        .unwrap_or_else(|_| format!("{}/{}", welcome.assigned_ip, welcome.prefix_len));

    match cfg.mode {
        SessionMode::GlobalProxy => {
            // Always pin the server's own address to the physical gateway so the
            // tunnel's carrier traffic never loops, regardless of split mode.
            if let Ok(orig_gw) = default_gateway().await {
                let host = server_ip.to_string();
                let _ = run("route", &["add", "-host", &host, &orig_gw]).await;
                undo.push(("route".into(), vec!["delete".into(), "-host".into(), host]));
            }

            if cfg.split_mode == crate::config::SplitMode::Whitelist {
                // Whitelist: capture nothing here — only the per-rule `via tunnel`
                // routes (installed above) use the tunnel; system DNS is untouched.
                tracing::info!("split mode = whitelist: only listed routes use {ifname}");
            } else {
                // Blacklist (default): capture ALL traffic + tunnel DNS.
                run("route", &["add", "-net", "0.0.0.0/1", "-interface", ifname]).await?;
                undo.push(("route".into(), vec!["delete".into(), "-net".into(), "0.0.0.0/1".into()]));
                run("route", &["add", "-net", "128.0.0.0/1", "-interface", ifname]).await?;
                undo.push(("route".into(), vec!["delete".into(), "-net".into(), "128.0.0.0/1".into()]));

                // IPv6 default capture (::/1 + 8000::/1), when the server offers v6.
                if welcome.assigned_ip6.is_some() {
                    if let IpAddr::V6(s6) = server_ip {
                        if let Some(gw6) = default_gateway6().await {
                            let _ = run("route", &["add", "-inet6", "-host", &s6.to_string(), &gw6]).await;
                            undo.push(("route".into(), vec!["delete".into(), "-inet6".into(), "-host".into(), s6.to_string()]));
                        }
                    }
                    let _ = run("route", &["add", "-inet6", "-net", "::/1", "-interface", ifname]).await;
                    undo.push(("route".into(), vec!["delete".into(), "-inet6".into(), "-net".into(), "::/1".into()]));
                    let _ = run("route", &["add", "-inet6", "-net", "8000::/1", "-interface", ifname]).await;
                    undo.push(("route".into(), vec!["delete".into(), "-inet6".into(), "-net".into(), "8000::/1".into()]));
                } else if !cfg.ipv6_killswitch {
                    tracing::warn!("IPv6 kill-switch OFF: server is v4-only, so native IPv6 will bypass the tunnel and may leak your real IP/location");
                } else if let Some(service) = &phys_service {
                    // IPv6 leak protection (kill-switch): the server is v4-only, so
                    // turn IPv6 OFF on the physical service — otherwise the host's
                    // NATIVE IPv6 (e.g. a China-Telecom 240e:: address) bypasses the
                    // tunnel and exposes the real IP/location. Restored on disconnect.
                    let _ = run("networksetup", &["-setv6off", service]).await;
                    undo.push(("networksetup".into(), vec!["-setv6automatic".into(), service.clone()]));
                    tracing::info!("IPv6 leak protection: server v4-only — IPv6 disabled on \"{service}\" while connected");
                } else {
                    tracing::warn!("server is v4-only but primary service not found; native IPv6 may leak");
                }

                if !welcome.dns.is_empty() || !welcome.dns6.is_empty() {
                    if let Ok(prev) = std::fs::read_to_string("/etc/resolv.conf") {
                        resolv_backup = Some(prev);
                    }
                    let body: String = welcome
                        .dns
                        .iter()
                        .map(|ip| format!("nameserver {ip}\n"))
                        .chain(welcome.dns6.iter().map(|ip| format!("nameserver {ip}\n")))
                        .collect();
                    let _ = write_in_place("/etc/resolv.conf", &body);

                    // The real fix on macOS: set the service DNS via networksetup
                    // (GUI apps consult SystemConfiguration, not resolv.conf). Save
                    // the previous list so the NetGuard can restore it on disconnect.
                    if let Some(service) = phys_service.clone() {
                        let prev = cmd_out("networksetup", &["-getdnsservers", &service])
                            .await
                            .unwrap_or_default();
                        let prev_list: Vec<String> = prev
                            .lines()
                            .map(|s| s.trim().to_string())
                            .filter(|s| s.parse::<IpAddr>().is_ok())
                            .collect();
                        let mut set_args = vec!["-setdnsservers".to_string(), service.clone()];
                        set_args.extend(welcome.dns.iter().map(|i| i.to_string()));
                        set_args.extend(welcome.dns6.iter().map(|i| i.to_string()));
                        let _ = run("networksetup", &set_args.iter().map(String::as_str).collect::<Vec<_>>()).await;
                        let mut restore = vec!["-setdnsservers".to_string(), service];
                        if prev_list.is_empty() {
                            restore.push("empty".to_string());
                        } else {
                            restore.extend(prev_list);
                        }
                        undo.push(("networksetup".into(), restore));
                        tracing::info!("DNS (networksetup) → v4 {:?} v6 {:?}", welcome.dns, welcome.dns6);
                    } else {
                        tracing::warn!("could not find primary network service; DNS set only in /etc/resolv.conf");
                    }
                }
            }
        }
        SessionMode::Vpn => {
            let _ = run("route", &["add", "-net", &subnet, "-interface", ifname]).await;
            undo.push(("route".into(), vec!["delete".into(), "-net".into(), subnet]));
            tracing::info!("VPN mode: virtual subnet reachable via {ifname}");
        }
        // No OS routing for proxy modes (the OS-proxy switch is handled by the
        // engine's sysproxy guard, not here).
        SessionMode::HttpProxy | SessionMode::SystemProxy => {}
    }

    Ok(NetGuard { undo, resolv_backup })
}

/// Bring up the utun and route *only* the virtual subnet through it — used when a
/// proxy-mode client also joins the VPN peer LAN. No default route / DNS change,
/// so internet traffic keeps following the proxy mode.
#[cfg(target_os = "macos")]
pub async fn apply_vpn_lan(welcome: &Welcome, ifname: &str) -> Result<NetGuard> {
    let mut undo: Vec<(String, Vec<String>)> = Vec::new();
    let ip = welcome.assigned_ip.to_string();
    let gw = welcome.gateway.to_string();
    let mtu = welcome.mtu.to_string();
    run("ifconfig", &[ifname, "inet", &ip, &gw, "up"]).await?;
    let _ = run("ifconfig", &[ifname, "mtu", &mtu]).await;
    let subnet = ipnet::Ipv4Net::new(welcome.assigned_ip, welcome.prefix_len)
        .map(|n| n.network())
        .map(|net| format!("{net}/{}", welcome.prefix_len))
        .unwrap_or_else(|_| format!("{}/{}", welcome.assigned_ip, welcome.prefix_len));
    // Best-effort (like VPN-mode `apply`): a leftover route from a prior run must
    // not fail the whole connect. The utun device drop also removes its routes.
    let _ = run("route", &["add", "-net", &subnet, "-interface", ifname]).await;
    undo.push(("route".into(), vec!["delete".into(), "-net".into(), subnet]));
    tracing::info!("VPN LAN: virtual subnet reachable via {ifname} (proxy mode + join)");
    Ok(NetGuard { undo, resolv_backup: None })
}

/// Install the per-destination split-tunnel routes (macOS).
///
/// `peer_gw` is the assigned utun peer gateway (`Welcome.gateway`); a `tunnel`
/// rule must route via it, not `-interface utunN`, since utun is point-to-point.
#[cfg(target_os = "macos")]
async fn apply_route_rules(
    cfg: &ClientConfig,
    _tun_ifname: &str,
    peer_gw: &str,
    orig_gw: &Option<String>,
    server_ip: IpAddr,
    undo: &mut Vec<(String, Vec<String>)>,
) {
    let server_route = format!("{server_ip}/32");
    let mut seen = std::collections::HashSet::new();
    for rule in &cfg.routes {
        for route in resolve_rule_targets(&rule.target).await {
            if route == server_route {
                tracing::warn!(
                    "route rule {} resolves to the server IP {server_ip}; skipped to keep the link up",
                    rule.target
                );
                continue;
            }
            if !seen.insert(route.clone()) {
                tracing::warn!("route {route} already claimed by an earlier rule; later rule ignored");
                continue;
            }

            // add args; undo mirrors them with add→delete for an exact removal.
            let mut add: Vec<String> = vec!["add".into(), "-net".into(), route.clone()];
            match rule.via.as_str() {
                "tunnel" => add.push(peer_gw.to_string()),
                "direct" => match orig_gw {
                    Some(gw) => add.push(gw.clone()),
                    None => {
                        tracing::warn!("route rule {} via direct: no default gateway", rule.target);
                        continue;
                    }
                },
                iface => match &rule.gateway {
                    // Bind to the NIC with -ifscope so it can't egress elsewhere.
                    Some(gw) => add.extend([gw.to_string(), "-ifscope".into(), iface.to_string()]),
                    None => add.extend(["-interface".into(), iface.to_string()]),
                },
            }
            let argref: Vec<&str> = add.iter().map(String::as_str).collect();
            match run("route", &argref).await {
                Ok(()) => {
                    tracing::info!(target = %rule.target, route = %route, via = %rule.via, "split-route applied");
                    let mut del = add.clone();
                    del[0] = "delete".into();
                    undo.push(("route".into(), del));
                }
                Err(e) => tracing::warn!("route rule {route} via {}: {e}", rule.via),
            }
        }
    }
}

// ----------------------------------------------------------------------------
// Windows (Wintun) — `netsh` + `route`. Wintun gives us the adapter but not its
// IP, so we assign it here, then install the same split-default capture the
// other OSes use. DNS is set on the tun adapter (removed when it's torn down);
// no /etc/resolv.conf. Compiles on Windows CI; runtime-tested by the user.
// ----------------------------------------------------------------------------
#[cfg(target_os = "windows")]
async fn run(cmd: &str, args: &[&str]) -> Result<()> {
    let out = tokio::process::Command::new(cmd)
        .args(args)
        .output()
        .await
        .map_err(|e| Error::Transport(format!("spawn `{cmd}`: {e}")))?;
    if !out.status.success() {
        return Err(Error::Transport(format!(
            "`{cmd} {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(())
}

#[cfg(target_os = "windows")]
async fn cmd_out(cmd: &str, args: &[&str]) -> Option<String> {
    let out = tokio::process::Command::new(cmd).args(args).output().await.ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Dotted IPv4 netmask for a prefix length (24 → 255.255.255.0).
#[cfg(target_os = "windows")]
fn prefix_to_mask(prefix: u8) -> String {
    let bits: u32 = if prefix == 0 {
        0
    } else if prefix >= 32 {
        u32::MAX
    } else {
        u32::MAX << (32 - prefix)
    };
    std::net::Ipv4Addr::from(bits).to_string()
}

/// Split an IPv4 CIDR ("10.0.0.0/8") into (network, dotted-mask) for `route add`.
#[cfg(target_os = "windows")]
fn split_cidr(cidr: &str) -> Option<(String, String)> {
    let net: ipnet::Ipv4Net = cidr.parse().ok()?;
    Some((net.network().to_string(), net.netmask().to_string()))
}

/// The IPv4 interface index of adapter `name`, from
/// `netsh interface ipv4 show interfaces` (columns: Idx Met MTU State Name…).
#[cfg(target_os = "windows")]
async fn if_index(name: &str) -> Option<u32> {
    let out = cmd_out("netsh", &["interface", "ipv4", "show", "interfaces"]).await?;
    for line in out.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 5 {
            continue;
        }
        let Ok(idx) = cols[0].parse::<u32>() else { continue };
        // The adapter name is the 5th column onward (it may contain spaces).
        if cols[4..].join(" ") == name {
            return Some(idx);
        }
    }
    None
}

/// The IPv4 default gateway, parsed from `route print -4 0.0.0.0`.
#[cfg(target_os = "windows")]
async fn default_gateway() -> Option<String> {
    let out = cmd_out("route", &["print", "-4", "0.0.0.0"]).await?;
    for line in out.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() >= 3 && cols[0] == "0.0.0.0" && cols[1] == "0.0.0.0" && cols[2] != "On-link" {
            if cols[2].parse::<std::net::Ipv4Addr>().is_ok() {
                return Some(cols[2].to_string());
            }
        }
    }
    None
}

/// Assign the wintun adapter's IPv4 address + MTU. Wintun creates the adapter
/// but doesn't address it; this also creates the on-link subnet route. Retries
/// briefly because the adapter can take a moment to become addressable.
#[cfg(target_os = "windows")]
async fn set_adapter_ipv4(ifname: &str, ip: &str, mask: &str, mtu: &str) -> Result<()> {
    let mut ok = false;
    for _ in 0..10 {
        if run(
            "netsh",
            &["interface", "ipv4", "set", "address", ifname, "static", ip, mask],
        )
        .await
        .is_ok()
        {
            ok = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
    if !ok {
        return Err(Error::Transport(format!(
            "could not assign {ip} {mask} to {ifname} via netsh"
        )));
    }
    let mtu_arg = format!("mtu={mtu}");
    let _ = run(
        "netsh",
        &["interface", "ipv4", "set", "subinterface", ifname, &mtu_arg, "store=active"],
    )
    .await;
    Ok(())
}

#[cfg(target_os = "windows")]
pub async fn apply(
    cfg: &ClientConfig,
    welcome: &Welcome,
    ifname: &str,
    server_ip: IpAddr,
) -> Result<NetGuard> {
    use entrotunnel_core::config::SessionMode;

    let mut undo: Vec<(String, Vec<String>)> = Vec::new();
    let resolv_backup: Option<String> = None; // Windows uses adapter DNS, not resolv.conf

    let ip = welcome.assigned_ip.to_string();
    let mask = prefix_to_mask(welcome.prefix_len);
    let mtu = welcome.mtu.to_string();

    set_adapter_ipv4(ifname, &ip, &mask, &mtu).await?;

    // IPv6 address (NAT66) when the server offers v6 (default /64 on-link prefix).
    if let Some(ip6) = welcome.assigned_ip6 {
        let a = ip6.to_string();
        let _ = run("netsh", &["interface", "ipv6", "add", "address", ifname, &a]).await;
    }

    let idx = if_index(ifname).await;
    let orig_gw = default_gateway().await;
    let tun_gw = welcome.gateway.to_string();

    apply_route_rules(cfg, ifname, idx, &tun_gw, &orig_gw, server_ip, &mut undo).await;

    match cfg.mode {
        SessionMode::GlobalProxy => {
            // Pin the server's own address to the physical gateway so the tunnel's
            // carrier traffic never loops.
            if let Some(gw) = &orig_gw {
                let s = server_ip.to_string();
                if run("route", &["add", &s, "mask", "255.255.255.255", gw]).await.is_ok() {
                    undo.push(("route".into(), vec!["delete".into(), s]));
                }
            } else {
                tracing::warn!("no default gateway; server pin route skipped");
            }

            if cfg.split_mode == crate::config::SplitMode::Whitelist {
                tracing::info!("split mode = whitelist: only listed routes use {ifname}");
            } else if let Some(idx) = idx {
                let idxs = idx.to_string();
                // Capture ALL v4 with two /1 routes via the tun (beat the 0/0 default).
                for net in ["0.0.0.0", "128.0.0.0"] {
                    if run("route", &["add", net, "mask", "128.0.0.0", &tun_gw, "metric", "1", "if", &idxs])
                        .await
                        .is_ok()
                    {
                        undo.push((
                            "route".into(),
                            vec!["delete".into(), net.into(), "mask".into(), "128.0.0.0".into(), tun_gw.clone()],
                        ));
                    } else {
                        tracing::warn!("failed to add v4 capture route {net}/1 via {ifname}");
                    }
                }

                // IPv6: route global v6 (::/1 + 8000::/1) at the tun — carrying it
                // through the tunnel (NAT66) when the server offers v6, or
                // blackholing it (kill-switch) when the server is v4-only so the
                // host's native IPv6 can't leak the real IP. Skip if the carrier
                // itself is v6 (would loop it into the tunnel).
                if matches!(server_ip, IpAddr::V6(_)) {
                    tracing::warn!("server reached over IPv6; skipping v6 capture/kill-switch to avoid looping the carrier");
                } else if welcome.assigned_ip6.is_some() || cfg.ipv6_killswitch {
                    for net6 in ["::/1", "8000::/1"] {
                        if run("netsh", &["interface", "ipv6", "add", "route", net6, ifname, "store=active"])
                            .await
                            .is_ok()
                        {
                            undo.push((
                                "netsh".into(),
                                vec![
                                    "interface".into(), "ipv6".into(), "delete".into(), "route".into(),
                                    net6.into(), ifname.to_string(),
                                ],
                            ));
                        }
                    }
                    if welcome.assigned_ip6.is_some() {
                        tracing::info!("IPv6 routed through the tunnel (NAT66)");
                    } else {
                        tracing::info!("IPv6 leak protection: server v4-only — native IPv6 blackholed while connected");
                    }
                } else {
                    tracing::warn!("IPv6 kill-switch OFF: server is v4-only, so native IPv6 will bypass the tunnel and may leak your real IP/location");
                }

                // DNS on the tun adapter (removed automatically when the adapter is
                // torn down on disconnect, so no restore command is needed).
                // `set` replaces the list with the primary; `add index=N` appends.
                for (i, ns) in welcome.dns.iter().enumerate() {
                    let addr = ns.to_string();
                    let _ = if i == 0 {
                        run("netsh", &["interface", "ipv4", "set", "dnsservers", ifname, "static", &addr, "validate=no"]).await
                    } else {
                        let idxarg = format!("index={}", i + 1);
                        run("netsh", &["interface", "ipv4", "add", "dnsservers", ifname, &addr, &idxarg, "validate=no"]).await
                    };
                }
                for (i, ns) in welcome.dns6.iter().enumerate() {
                    let addr = ns.to_string();
                    let _ = if i == 0 {
                        run("netsh", &["interface", "ipv6", "set", "dnsservers", ifname, "static", &addr, "validate=no"]).await
                    } else {
                        let idxarg = format!("index={}", i + 1);
                        run("netsh", &["interface", "ipv6", "add", "dnsservers", ifname, &addr, &idxarg, "validate=no"]).await
                    };
                }
                if !welcome.dns.is_empty() || !welcome.dns6.is_empty() {
                    tracing::info!("DNS → v4 {:?} v6 {:?}", welcome.dns, welcome.dns6);
                }
            } else {
                tracing::warn!("could not resolve {ifname} interface index; global capture routes skipped");
            }
        }
        SessionMode::Vpn => {
            tracing::info!("VPN mode: virtual subnet reachable via {ifname}");
        }
        // No OS routing for proxy modes (the OS-proxy switch is the sysproxy guard).
        SessionMode::HttpProxy | SessionMode::SystemProxy => {}
    }

    Ok(NetGuard { undo, resolv_backup })
}

/// Bring up the wintun adapter and route *only* the virtual subnet through it —
/// used when a proxy-mode client also joins the VPN peer LAN. Assigning the
/// address creates the on-link subnet route; the adapter drop removes it.
#[cfg(target_os = "windows")]
pub async fn apply_vpn_lan(welcome: &Welcome, ifname: &str) -> Result<NetGuard> {
    let undo: Vec<(String, Vec<String>)> = Vec::new();
    let ip = welcome.assigned_ip.to_string();
    let mask = prefix_to_mask(welcome.prefix_len);
    let mtu = welcome.mtu.to_string();
    set_adapter_ipv4(ifname, &ip, &mask, &mtu).await?;
    tracing::info!("VPN LAN: virtual subnet reachable via {ifname} (proxy mode + join)");
    Ok(NetGuard { undo, resolv_backup: None })
}

/// Install the per-destination split-tunnel routes (Windows).
#[cfg(target_os = "windows")]
async fn apply_route_rules(
    cfg: &ClientConfig,
    _tun_ifname: &str,
    tun_idx: Option<u32>,
    tun_gw: &str,
    orig_gw: &Option<String>,
    server_ip: IpAddr,
    undo: &mut Vec<(String, Vec<String>)>,
) {
    let server_route = format!("{server_ip}/32");
    let mut seen = std::collections::HashSet::new();
    for rule in &cfg.routes {
        for route in resolve_rule_targets(&rule.target).await {
            if route == server_route {
                tracing::warn!(
                    "route rule {} resolves to the server IP {server_ip}; skipped to keep the link up",
                    rule.target
                );
                continue;
            }
            if !seen.insert(route.clone()) {
                tracing::warn!("route {route} already claimed by an earlier rule; later rule ignored");
                continue;
            }
            let Some((dest, mask)) = split_cidr(&route) else {
                tracing::warn!("route rule {route}: cannot parse as an IPv4 CIDR; skipped");
                continue;
            };

            // `route add <dest> mask <mask> <gw> [metric 1] [if <idx>]`.
            let mut add: Vec<String> = vec!["add".into(), dest.clone(), "mask".into(), mask.clone()];
            match rule.via.as_str() {
                "tunnel" => match tun_idx {
                    Some(i) => add.extend([tun_gw.to_string(), "metric".into(), "1".into(), "if".into(), i.to_string()]),
                    None => {
                        tracing::warn!("route rule {} via tunnel: no tun interface index", rule.target);
                        continue;
                    }
                },
                "direct" => match orig_gw {
                    Some(gw) => add.push(gw.clone()),
                    None => {
                        tracing::warn!("route rule {} via direct: no default gateway", rule.target);
                        continue;
                    }
                },
                iface => match if_index(iface).await {
                    Some(i) => {
                        let gw = rule.gateway.as_ref().map(|g| g.to_string()).unwrap_or_else(|| "0.0.0.0".to_string());
                        add.extend([gw, "if".into(), i.to_string()]);
                    }
                    None => {
                        tracing::warn!("route rule {}: interface {iface} not found", rule.target);
                        continue;
                    }
                },
            }
            let argref: Vec<&str> = add.iter().map(String::as_str).collect();
            match run("route", &argref).await {
                Ok(()) => {
                    tracing::info!(target = %rule.target, route = %route, via = %rule.via, "split-route applied");
                    undo.push(("route".into(), vec!["delete".into(), dest, "mask".into(), mask]));
                }
                Err(e) => tracing::warn!("route rule {route} via {}: {e}", rule.via),
            }
        }
    }
}

// ----------------------------------------------------------------------------
// Other platforms — scaffold
// ----------------------------------------------------------------------------
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub async fn apply(
    _cfg: &ClientConfig,
    _welcome: &Welcome,
    _ifname: &str,
    _server_ip: IpAddr,
) -> Result<NetGuard> {
    Err(Error::NotImplemented("network configuration is not implemented on this OS"))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub async fn apply_vpn_lan(_welcome: &Welcome, _ifname: &str) -> Result<NetGuard> {
    Err(Error::NotImplemented("VPN LAN routing is not implemented on this OS"))
}
