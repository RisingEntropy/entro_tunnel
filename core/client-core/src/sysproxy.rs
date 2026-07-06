//! OS-wide **system proxy** toggling for `SessionMode::SystemProxy`.
//!
//! System-proxy mode runs the same local HTTP proxy as HTTP-proxy mode, but also
//! points the operating system's proxy settings at it — so proxy-aware apps use
//! the tunnel automatically while everything else is left alone (an intensity
//! between HTTP-proxy and full TUN capture, à la Clash Verge's "System Proxy").
//!
//! [`enable`] is best-effort: it never fails the connection. It returns a
//! [`SysProxyGuard`] whose `Drop` restores the previous state synchronously.

use tracing::{info, warn};

/// Restores the OS proxy settings when dropped.
pub struct SysProxyGuard {
    /// `(command, args)` pairs run on cleanup, in order. Empty = nothing set.
    undo: Vec<(String, Vec<String>)>,
}

impl SysProxyGuard {
    fn noop() -> Self {
        SysProxyGuard { undo: Vec::new() }
    }
}

impl Drop for SysProxyGuard {
    fn drop(&mut self) {
        if self.undo.is_empty() {
            return;
        }
        for (cmd, args) in &self.undo {
            let _ = std::process::Command::new(cmd).args(args).status();
        }
        info!("system proxy disabled (OS settings restored)");
    }
}

/// Split `host:port` (e.g. `127.0.0.1:7890`) into a host the OS should dial and
/// the port. A wildcard/empty bind is reported to the OS as loopback.
fn host_port(listen: &str) -> (String, String) {
    let (h, p) = listen.rsplit_once(':').unwrap_or((listen, "7890"));
    let host = if h.is_empty() || h == "0.0.0.0" || h == "::" || h == "[::]" {
        "127.0.0.1".to_string()
    } else {
        h.trim_matches(|c| c == '[' || c == ']').to_string()
    };
    (host, p.to_string())
}

/// Point the OS system proxy at the local HTTP listener `listen`.
pub fn enable(listen: &str) -> SysProxyGuard {
    let (host, port) = host_port(listen);
    enable_impl(&host, &port)
}

// ---------------------------------------------------------------------------
// macOS — `networksetup` on the primary network service.
// ---------------------------------------------------------------------------
#[cfg(target_os = "macos")]
fn enable_impl(host: &str, port: &str) -> SysProxyGuard {
    let Some(svc) = primary_service() else {
        warn!("system proxy: could not determine the primary network service; skipped");
        return SysProxyGuard::noop();
    };
    let ok_web = run("networksetup", &["-setwebproxy", &svc, host, port]);
    let ok_sec = run("networksetup", &["-setsecurewebproxy", &svc, host, port]);
    if !(ok_web && ok_sec) {
        warn!(
            "system proxy: networksetup failed for service \"{svc}\" \
             (admin privileges may be required); the local proxy is still up"
        );
        return SysProxyGuard::noop();
    }
    info!("system proxy → {host}:{port} on \"{svc}\" (HTTP + HTTPS)");
    SysProxyGuard {
        undo: vec![
            ("networksetup".into(), vec!["-setwebproxystate".into(), svc.clone(), "off".into()]),
            ("networksetup".into(), vec!["-setsecurewebproxystate".into(), svc, "off".into()]),
        ],
    }
}

/// Resolve the network *service* name (e.g. "Wi-Fi") backing the default-route
/// interface (e.g. `en0`), which `networksetup` addresses by service name.
#[cfg(target_os = "macos")]
fn primary_service() -> Option<String> {
    // 1. default-route interface.
    let route = std::process::Command::new("route").args(["-n", "get", "default"]).output().ok()?;
    let route = String::from_utf8_lossy(&route.stdout);
    let dev = route
        .lines()
        .find_map(|l| l.trim().strip_prefix("interface:"))
        .map(|s| s.trim().to_string())?;
    // 2. map interface → service name via the service order listing.
    let order = std::process::Command::new("networksetup")
        .arg("-listnetworkserviceorder")
        .output()
        .ok()?;
    let order = String::from_utf8_lossy(&order.stdout);
    // Blocks look like:  "(1) Wi-Fi\n(Hardware Port: Wi-Fi, Device: en0)\n"
    let mut last_service: Option<String> = None;
    for line in order.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix('(') {
            if let Some(idx) = rest.find(')') {
                // "(1) Wi-Fi" → service name after ") "
                if !rest[..idx].contains("Hardware Port") {
                    last_service = Some(rest[idx + 1..].trim().to_string());
                    continue;
                }
            }
        }
        if t.contains(&format!("Device: {dev}")) {
            return last_service.clone();
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Linux — best-effort GNOME (`gsettings`). Only affects proxy-aware GNOME apps.
// ---------------------------------------------------------------------------
#[cfg(target_os = "linux")]
fn enable_impl(host: &str, port: &str) -> SysProxyGuard {
    let g = |args: &[&str]| run("gsettings", args);
    let base = "org.gnome.system.proxy";
    if !g(&["set", base, "mode", "manual"]) {
        warn!("system proxy: gsettings unavailable (non-GNOME?); the local proxy is still up");
        return SysProxyGuard::noop();
    }
    for proto in ["http", "https"] {
        let schema = format!("{base}.{proto}");
        g(&["set", &schema, "host", host]);
        g(&["set", &schema, "port", port]);
    }
    info!("system proxy → {host}:{port} (GNOME gsettings, HTTP + HTTPS)");
    SysProxyGuard {
        undo: vec![("gsettings".into(), vec!["set".into(), base.into(), "mode".into(), "none".into()])],
    }
}

// ---------------------------------------------------------------------------
// Windows — WinINet proxy via the HKCU registry, refreshed so it takes effect
// without a logout. Proxy-aware apps (Edge/Chrome, most system HTTP) honour it.
// ---------------------------------------------------------------------------
#[cfg(target_os = "windows")]
const INET_SETTINGS: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings";

// Tell WinINet its settings changed + flush its cache (INTERNET_OPTION_SETTINGS_
// CHANGED=39, INTERNET_OPTION_REFRESH=37) via a tiny P/Invoke — done in
// PowerShell so there is no compile-time FFI to maintain.
#[cfg(target_os = "windows")]
const WININET_REFRESH: &str = "Add-Type -Namespace ET -Name WinInet -MemberDefinition '[System.Runtime.InteropServices.DllImport(\"wininet.dll\", SetLastError=true)] public static extern bool InternetSetOption(System.IntPtr h, int o, System.IntPtr b, int l);'; [void][ET.WinInet]::InternetSetOption([System.IntPtr]::Zero,39,[System.IntPtr]::Zero,0); [void][ET.WinInet]::InternetSetOption([System.IntPtr]::Zero,37,[System.IntPtr]::Zero,0)";

#[cfg(target_os = "windows")]
fn refresh_wininet() {
    run("powershell", &["-NoProfile", "-NonInteractive", "-Command", WININET_REFRESH]);
}

#[cfg(target_os = "windows")]
fn enable_impl(host: &str, port: &str) -> SysProxyGuard {
    let server = format!("{host}:{port}");
    let ok_srv = run("reg", &["add", INET_SETTINGS, "/v", "ProxyServer", "/t", "REG_SZ", "/d", &server, "/f"]);
    // Don't proxy loopback / plain intranet names.
    let _ = run("reg", &["add", INET_SETTINGS, "/v", "ProxyOverride", "/t", "REG_SZ", "/d", "localhost;127.*;<local>", "/f"]);
    let ok_en = run("reg", &["add", INET_SETTINGS, "/v", "ProxyEnable", "/t", "REG_DWORD", "/d", "1", "/f"]);
    if !(ok_srv && ok_en) {
        warn!("system proxy: could not write WinINet registry settings; the local proxy is still up");
        return SysProxyGuard::noop();
    }
    refresh_wininet();
    info!("system proxy → {server} (WinINet, HTTP + HTTPS)");
    SysProxyGuard {
        undo: vec![
            ("reg".into(), vec!["add".into(), INET_SETTINGS.into(), "/v".into(), "ProxyEnable".into(), "/t".into(), "REG_DWORD".into(), "/d".into(), "0".into(), "/f".into()]),
            ("powershell".into(), vec!["-NoProfile".into(), "-NonInteractive".into(), "-Command".into(), WININET_REFRESH.into()]),
        ],
    }
}

// ---------------------------------------------------------------------------
// Other platforms — not yet implemented (the local proxy still runs).
// ---------------------------------------------------------------------------
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn enable_impl(_host: &str, _port: &str) -> SysProxyGuard {
    warn!("system proxy: not implemented on this OS; set the proxy manually (the local HTTP proxy is up)");
    SysProxyGuard::noop()
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
fn run(cmd: &str, args: &[&str]) -> bool {
    std::process::Command::new(cmd)
        .args(args)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
