//! Server-side network plumbing: bring up the shared TUN, enable IP forwarding,
//! and install NAT (MASQUERADE) + FORWARD rules so the kernel handles egress
//! and peer-to-peer routing. Linux only.

use crate::config::NetworkConfig;
use entrotunnel_core::tun::TunDevice;
use entrotunnel_core::{Error, Result};
use std::sync::Arc;

/// Holds the live TUN and the cleanup commands to undo firewall/forwarding
/// changes when dropped.
pub struct ServerNet {
    pub tun: Arc<TunDevice>,
    /// Egress interface chosen for NAT (kept for diagnostics/cleanup).
    #[allow(dead_code)]
    pub egress: String,
    /// True when IPv6 NAT66 was set up (subnet6 configured AND v6 egress found).
    /// Gates whether the server advertises v6 to clients in the Welcome.
    pub ipv6: bool,
    undo: Vec<(String, Vec<String>)>,
}

impl Drop for ServerNet {
    fn drop(&mut self) {
        for (cmd, args) in &self.undo {
            let _ = std::process::Command::new(cmd).args(args).status();
        }
    }
}

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
async fn detect_egress() -> Result<String> {
    let out = tokio::process::Command::new("ip")
        .args(["route", "show", "default"])
        .output()
        .await
        .map_err(|e| Error::Transport(format!("ip route: {e}")))?;
    let text = String::from_utf8_lossy(&out.stdout);
    let toks: Vec<&str> = text.split_whitespace().collect();
    toks.iter()
        .position(|t| *t == "dev")
        .and_then(|i| toks.get(i + 1))
        .map(|s| s.to_string())
        .ok_or_else(|| Error::Transport("could not detect egress interface".into()))
}

/// The interface carrying the IPv6 default route, if the host has IPv6 egress.
/// Returns `None` when there is no global IPv6 connectivity (so v6 is skipped).
#[cfg(target_os = "linux")]
async fn detect_egress6() -> Option<String> {
    let out = tokio::process::Command::new("ip")
        .args(["-6", "route", "show", "default"])
        .output()
        .await
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let toks: Vec<&str> = text.split_whitespace().collect();
    toks.iter()
        .position(|t| *t == "dev")
        .and_then(|i| toks.get(i + 1))
        .map(|s| s.to_string())
}

#[cfg(target_os = "linux")]
pub async fn setup(net: &NetworkConfig, prefix: u8, subnet: &str) -> Result<ServerNet> {
    let tun = Arc::new(
        TunDevice::create(&entrotunnel_core::tun::TunConfig {
            name: net.tun_name.clone(),
            ip: net.gateway,
            prefix_len: prefix,
            mtu: net.mtu,
        })
        .await?,
    );
    let ifname = tun.name().to_string();

    run("ip", &["addr", "add", &format!("{}/{prefix}", net.gateway), "dev", &ifname]).await.ok();
    run("ip", &["link", "set", "dev", &ifname, "mtu", &net.mtu.to_string()]).await?;
    run("ip", &["link", "set", "dev", &ifname, "up"]).await?;
    run("sysctl", &["-w", "net.ipv4.ip_forward=1"]).await.ok();
    // Suppress ICMP redirects on the TUN: peers can't route to each other
    // directly, so the gateway must keep forwarding their hairpin traffic.
    run("sysctl", &["-w", &format!("net.ipv4.conf.{ifname}.send_redirects=0")]).await.ok();
    run("sysctl", &["-w", "net.ipv4.conf.all.send_redirects=0"]).await.ok();

    let egress = match &net.egress_iface {
        Some(e) => e.clone(),
        None => detect_egress().await?,
    };
    tracing::info!(tun = %ifname, egress = %egress, subnet, "server network configured");

    let mut undo: Vec<(String, Vec<String>)> = Vec::new();

    // NAT: MASQUERADE virtual-subnet traffic leaving via the real interface.
    let masq = |action: &str| {
        vec![
            "-t".to_string(), "nat".into(), action.into(), "POSTROUTING".into(),
            "-s".into(), subnet.to_string(), "-o".into(), egress.clone(),
            "-j".into(), "MASQUERADE".into(),
        ]
    };
    let add = masq("-A");
    run("iptables", &add.iter().map(String::as_str).collect::<Vec<_>>()).await.ok();
    undo.push(("iptables".into(), masq("-D")));

    // Allow forwarding both directions between TUN and egress.
    let fwd_out = |action: &str| {
        vec![action.to_string(), "FORWARD".into(), "-i".into(), ifname.clone(),
             "-o".into(), egress.clone(), "-j".into(), "ACCEPT".into()]
    };
    let fwd_in = |action: &str| {
        vec![action.to_string(), "FORWARD".into(), "-i".into(), egress.clone(),
             "-o".into(), ifname.clone(), "-m".into(), "state".into(),
             "--state".into(), "RELATED,ESTABLISHED".into(), "-j".into(), "ACCEPT".into()]
    };
    // Hairpin: peer-to-peer VPN traffic enters and leaves on the same TUN.
    let fwd_hairpin = |action: &str| {
        vec![action.to_string(), "FORWARD".into(), "-i".into(), ifname.clone(),
             "-o".into(), ifname.clone(), "-j".into(), "ACCEPT".into()]
    };
    for rule in [fwd_out("-I"), fwd_in("-I"), fwd_hairpin("-I")] {
        run("iptables", &rule.iter().map(String::as_str).collect::<Vec<_>>()).await.ok();
    }
    undo.push(("iptables".into(), fwd_out("-D")));
    undo.push(("iptables".into(), fwd_in("-D")));
    undo.push(("iptables".into(), fwd_hairpin("-D")));

    // --- IPv6 (NAT66) — only when a ULA subnet is configured AND the host has
    // working IPv6 egress. Everything here is best-effort: a box without v6 (or
    // without ip6tables) just stays v4-only; the v4 path above is unaffected.
    let ipv6 = setup_ipv6(net, &ifname, &mut undo).await;

    Ok(ServerNet { tun, egress, ipv6, undo })
}

/// Bring up IPv6 on the TUN and install ip6tables NAT66 + forwarding. Returns
/// `true` only if v6 egress was found and the masquerade rule went in.
#[cfg(target_os = "linux")]
async fn setup_ipv6(
    net: &NetworkConfig,
    ifname: &str,
    undo: &mut Vec<(String, Vec<String>)>,
) -> bool {
    let (Some(subnet6), Some(gw6)) = (net.subnet6.clone(), net.gateway6) else {
        return false; // v6 disabled in config
    };
    let prefix6 = subnet6
        .parse::<ipnet::Ipv6Net>()
        .map(|n| n.prefix_len())
        .unwrap_or(64);
    let Some(egress6) = detect_egress6().await else {
        tracing::info!("no IPv6 default route; serving IPv4 only (set up v6 egress to enable NAT66)");
        return false;
    };

    // Address + forwarding on the TUN.
    run("ip", &["-6", "addr", "add", &format!("{gw6}/{prefix6}"), "dev", ifname]).await.ok();
    run("sysctl", &["-w", "net.ipv6.conf.all.forwarding=1"]).await.ok();
    run("sysctl", &["-w", &format!("net.ipv6.conf.{ifname}.forwarding=1")]).await.ok();

    // NAT66: MASQUERADE the ULA subnet out the real v6 interface.
    let masq = |action: &str| {
        vec![
            "-t".to_string(), "nat".into(), action.into(), "POSTROUTING".into(),
            "-s".into(), subnet6.clone(), "-o".into(), egress6.clone(),
            "-j".into(), "MASQUERADE".into(),
        ]
    };
    if run("ip6tables", &masq("-A").iter().map(String::as_str).collect::<Vec<_>>())
        .await
        .is_err()
    {
        tracing::warn!("ip6tables NAT66 setup failed (no ip6tables?); serving IPv4 only");
        // Best-effort: try to remove the address we added so we don't leave it dangling.
        run("ip", &["-6", "addr", "del", &format!("{gw6}/{prefix6}"), "dev", ifname]).await.ok();
        return false;
    }
    undo.push(("ip6tables".into(), masq("-D")));

    // FORWARD both directions + peer-to-peer hairpin (mirrors the v4 rules).
    let fwd_out = |a: &str| vec![a.to_string(), "FORWARD".into(), "-i".into(), ifname.to_string(),
        "-o".into(), egress6.clone(), "-j".into(), "ACCEPT".into()];
    let fwd_in = |a: &str| vec![a.to_string(), "FORWARD".into(), "-i".into(), egress6.clone(),
        "-o".into(), ifname.to_string(), "-m".into(), "state".into(),
        "--state".into(), "RELATED,ESTABLISHED".into(), "-j".into(), "ACCEPT".into()];
    let fwd_hp = |a: &str| vec![a.to_string(), "FORWARD".into(), "-i".into(), ifname.to_string(),
        "-o".into(), ifname.to_string(), "-j".into(), "ACCEPT".into()];
    for rule in [fwd_out("-I"), fwd_in("-I"), fwd_hp("-I")] {
        run("ip6tables", &rule.iter().map(String::as_str).collect::<Vec<_>>()).await.ok();
    }
    undo.push(("ip6tables".into(), fwd_out("-D")));
    undo.push(("ip6tables".into(), fwd_in("-D")));
    undo.push(("ip6tables".into(), fwd_hp("-D")));

    tracing::info!(subnet6 = %subnet6, egress6 = %egress6, "IPv6 NAT66 enabled");
    true
}

#[cfg(not(target_os = "linux"))]
pub async fn setup(_net: &NetworkConfig, _prefix: u8, _subnet: &str) -> Result<ServerNet> {
    Err(Error::NotImplemented(
        "server network setup is Linux-only (TUN + iptables)",
    ))
}
