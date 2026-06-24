//! EntroTunnel desktop app (Tauri v2).
//!
//! The backend is a thin command layer over `entrotunnel-client::Engine` — the
//! exact same engine the CLI drives. It persists two things:
//!
//! * `profiles.json` — a list of [`Profile`]s. A profile is *server config only*
//!   (the endpoints, tokens, crypto). It carries no mode: that is a local choice.
//! * `settings.json` — the device-local [`ConnectionSettings`] (which mode to run,
//!   TUN/HTTP parameters, split-tunnel routes) plus the active profile name.
//!
//! At connect time the two are composed into the engine's `ClientConfig`.
//!
//! NOTE: global-proxy / VPN modes create a TUN device and need elevated
//! privileges (root / Administrator).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// In-memory log ring buffer shown on the GUI's Logs page (newest last).
type SharedLogs = Arc<Mutex<VecDeque<String>>>;
/// Cap so a long-running session can't grow memory without bound.
const LOG_CAP: usize = 2000;

/// A `tracing` writer that tees formatted log lines to stderr (so the console
/// still works) AND into the shared ring buffer the Logs page polls. One writer
/// is made per event; on drop it splits the accumulated bytes into lines.
#[derive(Clone)]
struct LogTee {
    buf: SharedLogs,
}
struct LogTeeWriter {
    buf: SharedLogs,
    acc: Vec<u8>,
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogTee {
    type Writer = LogTeeWriter;
    fn make_writer(&'a self) -> Self::Writer {
        LogTeeWriter {
            buf: self.buf.clone(),
            acc: Vec::new(),
        }
    }
}
impl std::io::Write for LogTeeWriter {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        // Fully-qualified so no `Write` trait import is needed here.
        let _ = std::io::Write::write_all(&mut std::io::stderr(), b); // keep console output
        self.acc.extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        std::io::Write::flush(&mut std::io::stderr())
    }
}
impl Drop for LogTeeWriter {
    fn drop(&mut self) {
        if self.acc.is_empty() {
            return;
        }
        let text = String::from_utf8_lossy(&self.acc);
        if let Ok(mut q) = self.buf.lock() {
            for line in text.split('\n') {
                let line = line.trim_end();
                if line.is_empty() {
                    continue;
                }
                if q.len() >= LOG_CAP {
                    q.pop_front();
                }
                q.push_back(line.to_string());
            }
        }
    }
}

use entrotunnel_client::config::{ClientConfig, ConnectionSettings, Profile};
use entrotunnel_client::engine::EngineHandle;
use entrotunnel_client::Engine;
use entrotunnel_core::config::SessionMode;
use serde::{Deserialize, Serialize};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, WindowEvent};

#[derive(Default, Clone, Serialize)]
struct Status {
    connected: bool,
    profile: Option<String>,
    assigned_ip: Option<String>,
    mode: Option<String>,
    error: Option<String>,
    /// VPN peers on the same server (empty unless connected in VPN mode). Filled
    /// live by the `status` command from the engine's shared state.
    peers: Vec<Peer>,
    /// Payload bytes sent from this desktop client during the current session.
    up_bytes: u64,
    /// Payload bytes received by this desktop client during the current session.
    down_bytes: u64,
}

/// One VPN peer shown in the GUI: its virtual IP and friendly name.
#[derive(Default, Clone, Serialize)]
struct Peer {
    ip: String,
    name: String,
}

/// Device-local state persisted to `settings.json`: connection settings + which
/// profile is currently selected on the Home screen.
#[derive(Default, Clone, Serialize, Deserialize)]
struct LocalState {
    #[serde(default)]
    settings: ConnectionSettings,
    #[serde(default)]
    active_profile: Option<String>,
}

struct AppState {
    config_dir: PathBuf,
    profiles_path: PathBuf,
    settings_path: PathBuf,
    engine: Mutex<Option<EngineHandle>>,
    status: Mutex<Status>,
    /// Captured tracing output for the Logs page.
    logs: SharedLogs,
    /// Set only by the tray "Quit" item: closing the window otherwise hides it to
    /// the tray instead of exiting (see `main`).
    quitting: AtomicBool,
}

// ---- persistence ---------------------------------------------------------

fn load_profiles(path: &PathBuf) -> Vec<Profile> {
    let Some(text) = std::fs::read_to_string(path).ok() else {
        return Vec::new();
    };
    // Preferred format: a list of slim Profiles.
    if let Ok(list) = serde_json::from_str::<Vec<Profile>>(&text) {
        return list;
    }
    // Migration: older builds stored full ClientConfigs (with mode etc.). Keep
    // just the server-config portion so existing profiles survive the upgrade.
    if let Ok(old) = serde_json::from_str::<Vec<ClientConfig>>(&text) {
        return old
            .into_iter()
            .map(|c| Profile {
                name: c.name,
                selected_server: c.selected_server,
                servers: c.servers,
            })
            .collect();
    }
    Vec::new()
}

fn save_profiles(path: &PathBuf, list: &[Profile]) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let text = serde_json::to_string_pretty(list).map_err(|e| e.to_string())?;
    std::fs::write(path, text).map_err(|e| e.to_string())
}

fn load_state(path: &PathBuf) -> LocalState {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save_state(path: &PathBuf, s: &LocalState) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let text = serde_json::to_string_pretty(s).map_err(|e| e.to_string())?;
    std::fs::write(path, text).map_err(|e| e.to_string())
}

/// Make `name` unique within `list` by appending " (2)", " (3)", … if needed.
fn unique_name(name: &str, list: &[Profile]) -> String {
    if !list.iter().any(|p| p.name == name) {
        return name.to_string();
    }
    for n in 2.. {
        let candidate = format!("{name} ({n})");
        if !list.iter().any(|p| p.name == candidate) {
            return candidate;
        }
    }
    name.to_string()
}

// ---- profile commands ----------------------------------------------------

#[tauri::command]
fn list_profiles(state: tauri::State<AppState>) -> Vec<Profile> {
    load_profiles(&state.profiles_path)
}

#[tauri::command]
fn upsert_profile(state: tauri::State<AppState>, profile: Profile) -> Result<(), String> {
    let mut list = load_profiles(&state.profiles_path);
    match list.iter_mut().find(|p| p.name == profile.name) {
        Some(existing) => *existing = profile,
        None => list.push(profile),
    }
    save_profiles(&state.profiles_path, &list)
}

#[tauri::command]
fn remove_profile(state: tauri::State<AppState>, name: String) -> Result<(), String> {
    let mut list = load_profiles(&state.profiles_path);
    list.retain(|p| p.name != name);
    save_profiles(&state.profiles_path, &list)
}

/// Import a profile from a pasted `entro://…` link (as exported by the server
/// admin panel). Returns the stored profile (its name may be de-duplicated).
#[tauri::command]
fn import_profile(state: tauri::State<AppState>, link: String) -> Result<Profile, String> {
    let mut profile = Profile::decode_link(&link).map_err(|e| e.to_string())?;
    if profile.servers.is_empty() {
        return Err("the imported config has no servers".into());
    }
    if profile.name.trim().is_empty() {
        profile.name = "imported".into();
    }
    let mut list = load_profiles(&state.profiles_path);
    profile.name = unique_name(&profile.name, &list);
    list.push(profile.clone());
    save_profiles(&state.profiles_path, &list)?;
    Ok(profile)
}

/// Produce a shareable `entro://…` link for a stored profile.
#[tauri::command]
fn export_profile(state: tauri::State<AppState>, name: String) -> Result<String, String> {
    load_profiles(&state.profiles_path)
        .into_iter()
        .find(|p| p.name == name)
        .map(|p| p.encode_link())
        .ok_or_else(|| format!("profile '{name}' not found"))
}

/// Result of exporting a profile to a TOML file.
#[derive(Serialize)]
struct TomlExport {
    /// Absolute path of the written `.toml` file.
    path: String,
    /// Its contents (also shown in the GUI for copy).
    toml: String,
}

/// Export a profile as a complete, CLI-ready `client.toml` — the profile's
/// servers composed with the device's current connection settings (mode, routes,
/// split mode, …), i.e. *all* configuration. Written next to `profiles.json`.
#[tauri::command]
fn export_profile_toml(state: tauri::State<AppState>, name: String) -> Result<TomlExport, String> {
    let profile = load_profiles(&state.profiles_path)
        .into_iter()
        .find(|p| p.name == name)
        .ok_or_else(|| format!("profile '{name}' not found"))?;
    let local = load_state(&state.settings_path);
    let cfg = ClientConfig::compose(&profile, &local.settings);
    let toml = cfg.to_toml().map_err(|e| e.to_string())?;

    // Filesystem-safe filename from the profile name.
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let safe = if safe.is_empty() {
        "profile".to_string()
    } else {
        safe
    };
    std::fs::create_dir_all(&state.config_dir).map_err(|e| e.to_string())?;
    let path = state.config_dir.join(format!("{safe}.toml"));
    std::fs::write(&path, &toml).map_err(|e| e.to_string())?;

    Ok(TomlExport {
        path: path.to_string_lossy().to_string(),
        toml,
    })
}

/// Export ALL stored profiles into one TOML file (a `[[profiles]]` bundle),
/// written next to `profiles.json`. Returns the path + contents.
#[tauri::command]
fn export_all_profiles(state: tauri::State<AppState>) -> Result<TomlExport, String> {
    let profiles = load_profiles(&state.profiles_path);
    let toml = entrotunnel_client::config::ProfileBundle { profiles }
        .to_toml()
        .map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&state.config_dir).map_err(|e| e.to_string())?;
    let path = state.config_dir.join("all-profiles.toml");
    std::fs::write(&path, &toml).map_err(|e| e.to_string())?;
    Ok(TomlExport {
        path: path.to_string_lossy().to_string(),
        toml,
    })
}

/// Import profile(s) from TOML — either a `[[profiles]]` bundle (from "Export
/// all") or a single `client.toml` (from "Export TOML"). Same-name profiles are
/// overwritten. Returns how many were imported.
#[tauri::command]
fn import_profiles_toml(state: tauri::State<AppState>, toml: String) -> Result<usize, String> {
    let parsed =
        entrotunnel_client::config::profiles_from_toml(&toml).map_err(|e| e.to_string())?;
    let mut list = load_profiles(&state.profiles_path);
    let mut n = 0usize;
    for p in parsed {
        if p.name.trim().is_empty() || p.servers.is_empty() {
            continue; // skip malformed entries
        }
        match list.iter_mut().find(|x| x.name == p.name) {
            Some(existing) => *existing = p, // overwrite same name (upsert)
            None => list.push(p),
        }
        n += 1;
    }
    if n == 0 {
        return Err("no valid profiles found in the TOML".into());
    }
    save_profiles(&state.profiles_path, &list)?;
    Ok(n)
}

// ---- local settings ------------------------------------------------------

#[tauri::command]
fn get_state(state: tauri::State<AppState>) -> LocalState {
    load_state(&state.settings_path)
}

#[tauri::command]
fn set_settings(state: tauri::State<AppState>, settings: ConnectionSettings) -> Result<(), String> {
    let mut s = load_state(&state.settings_path);
    s.settings = settings;
    save_state(&state.settings_path, &s)
}

#[tauri::command]
fn set_active_profile(state: tauri::State<AppState>, name: Option<String>) -> Result<(), String> {
    let mut s = load_state(&state.settings_path);
    s.active_profile = name;
    save_state(&state.settings_path, &s)
}

#[tauri::command]
fn gen_psk() -> String {
    entrotunnel_core::config::generate_psk()
}

// ---- privilege elevation (TUN modes need root / Administrator) -------------

/// Whether this process can create a TUN device (root on unix, admin on Windows).
fn process_is_elevated() -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::geteuid() == 0 }
    }
    #[cfg(windows)]
    {
        is_elevated::is_elevated()
    }
    #[cfg(not(any(unix, windows)))]
    {
        false
    }
}

#[tauri::command]
fn is_elevated() -> bool {
    process_is_elevated()
}

/// Relaunch this app with admin/root rights via the OS authorization dialog,
/// passing the same config dir so the elevated instance keeps the user's
/// profiles. On success the current (unprivileged) instance exits so only the
/// elevated one remains.
#[tauri::command]
async fn relaunch_elevated(app: tauri::AppHandle) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let exe_s = exe.to_string_lossy().to_string();
    let dir_s = app
        .state::<AppState>()
        .config_dir
        .to_string_lossy()
        .to_string();

    // The auth/UAC dialog blocks, so run it off the async runtime.
    let launched = tauri::async_runtime::spawn_blocking(move || elevate_relaunch(&exe_s, &dir_s))
        .await
        .map_err(|e| e.to_string())?;
    launched?; // Err = the user cancelled the dialog → keep this instance running

    app.state::<AppState>()
        .quitting
        .store(true, Ordering::SeqCst);
    let app2 = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        app2.exit(0);
    });
    Ok(())
}

/// True if Touch ID is enabled for `sudo` (pam_tid.so present, uncommented, in
/// either the system /etc/pam.d/sudo or the update-safe /etc/pam.d/sudo_local).
/// When it is, we can elevate via `sudo` and the user gets a fingerprint sheet
/// instead of a password box.
#[cfg(target_os = "macos")]
fn touchid_sudo_enabled() -> bool {
    ["/etc/pam.d/sudo_local", "/etc/pam.d/sudo"]
        .iter()
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .any(|s| {
            s.lines()
                .map(str::trim_start)
                .any(|l| !l.starts_with('#') && l.contains("pam_tid.so"))
        })
}

#[cfg(target_os = "macos")]
fn elevate_relaunch(exe: &str, cfg_dir: &str) -> Result<(), String> {
    // We relaunch via `launchctl asuser <uid>` so the elevated instance is
    // bootstrapped into the user's GUI (Aqua) session — a plain root process has
    // no WindowServer access, which is why the menubar tray disappears and the
    // window can't be reopened. The process still runs as root, so utun/routing
    // work.
    let uid = unsafe { libc::getuid() }; // this (unprivileged) GUI's user
    let q = |s: &str| s.replace('\'', "'\\''"); // single-quote for /bin/sh
    let inner = format!(
        "launchctl asuser {uid} '{}' --config-dir '{}' >/dev/null 2>&1 &",
        q(exe),
        q(cfg_dir)
    );

    // If Touch ID for sudo is enabled, TRY `sudo` first: where the OS can present
    // the biometric prompt, the user authorizes with a fingerprint and we're done.
    // Caveat: `pam_tid` needs a controlling terminal to present the sheet, which a
    // Finder-launched GUI app usually lacks — so sudo often can't prompt here.
    // stdin=/dev/null makes that case fail FAST (never hangs), and we then fall
    // back to the AppleScript password dialog below instead of leaving the user
    // stuck (the bug that showed "authorization was cancelled" with no prompt).
    if touchid_sudo_enabled() {
        let ok = std::process::Command::new("/usr/bin/sudo")
            .args(["/bin/sh", "-c"])
            .arg(&inner)
            .stdin(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            return Ok(());
        }
        // sudo could not authorize (no fingerprint prompt available in this
        // context) — fall through to the reliable password dialog.
    }

    // Fallback: AppleScript's admin dialog. Apple routes this one through a
    // password field, not Touch ID, but it always works from a GUI app.
    let script = format!(
        "do shell script \"{}\" with administrator privileges",
        inner.replace('\\', "\\\\").replace('"', "\\\"")
    );
    let st = std::process::Command::new("osascript")
        .arg("-e")
        .arg(script)
        .status()
        .map_err(|e| e.to_string())?;
    if st.success() {
        Ok(())
    } else {
        Err("authorization was cancelled".into())
    }
}

#[cfg(target_os = "windows")]
fn elevate_relaunch(exe: &str, cfg_dir: &str) -> Result<(), String> {
    // `Start-Process -Verb RunAs` triggers the UAC prompt and launches elevated.
    let ps = format!(
        "Start-Process -FilePath '{}' -ArgumentList '--config-dir','{}' -Verb RunAs",
        exe.replace('\'', "''"),
        cfg_dir.replace('\'', "''")
    );
    let st = std::process::Command::new("powershell")
        .args(["-NoProfile", "-WindowStyle", "Hidden", "-Command", &ps])
        .status()
        .map_err(|e| e.to_string())?;
    if st.success() {
        Ok(())
    } else {
        Err("UAC elevation was cancelled".into())
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn elevate_relaunch(exe: &str, cfg_dir: &str) -> Result<(), String> {
    // Linux: graphical sudo via pkexec (best-effort).
    std::process::Command::new("pkexec")
        .arg(exe)
        .arg("--config-dir")
        .arg(cfg_dir)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("pkexec failed: {e} (install policykit, or run with sudo)"))
}

#[cfg(not(any(unix, windows)))]
fn elevate_relaunch(_exe: &str, _cfg_dir: &str) -> Result<(), String> {
    Err("elevation is not supported on this OS".into())
}

// ---- engine control ------------------------------------------------------

#[tauri::command]
async fn connect_profile(state: tauri::State<'_, AppState>, name: String) -> Result<(), String> {
    let profile = load_profiles(&state.profiles_path)
        .into_iter()
        .find(|p| p.name == name)
        .ok_or_else(|| format!("profile '{name}' not found"))?;
    let local = load_state(&state.settings_path);
    let cfg = ClientConfig::compose(&profile, &local.settings);

    // Persist the active profile so the Home screen restores it.
    {
        let mut s = local.clone();
        s.active_profile = Some(name.clone());
        let _ = save_state(&state.settings_path, &s);
    }

    // Stop any existing session first.
    let prev = { state.engine.lock().unwrap().take() };
    if let Some(h) = prev {
        let _ = h.stop().await;
    }

    let mode = cfg.mode.to_string();
    // Packet modes always create a TUN; joining the VPN from a proxy mode also
    // creates one — both need root/admin.
    let needs_tun = matches!(cfg.mode, SessionMode::GlobalProxy | SessionMode::Vpn) || cfg.join_vpn;
    let requested_ip = cfg.requested_ip.map(|ip| ip.to_string());
    let handle = Engine::start(cfg);

    // The engine runs in the background. Wait briefly so that an immediate setup
    // failure — bad token, TLS error, connection refused, or (very commonly) a
    // TUN device that needs root — is reported back instead of silently flipping
    // the UI from "connecting" to "disconnected" with no explanation.
    let mut waited = 0u64;
    let early_fail = loop {
        if handle.task.is_finished() {
            break true;
        }
        if waited >= 3000 {
            break false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        waited += 150;
    };

    if early_fail {
        let mut msg = match handle.task.await {
            Ok(Ok(())) => "the session ended immediately".to_string(),
            Ok(Err(e)) => e.to_string(),
            Err(e) => format!("engine task failed: {e}"),
        };
        // The classic macOS/Linux case: utun/tun needs elevated privileges.
        let low = msg.to_lowercase();
        if needs_tun
            && (low.contains("not permitted")
                || low.contains("denied")
                || low.contains("permission"))
        {
            msg = format!(
                "{msg} — Global proxy / VPN create a virtual NIC and need administrator/root. \
                 Run the app elevated, or switch to System proxy mode (no admin needed)."
            );
        }
        *state.status.lock().unwrap() = Status {
            connected: false,
            profile: Some(name),
            assigned_ip: None,
            mode: Some(mode),
            error: Some(msg.clone()),
            peers: Vec::new(),
            up_bytes: 0,
            down_bytes: 0,
        };
        return Err(msg);
    }

    *state.engine.lock().unwrap() = Some(handle);
    *state.status.lock().unwrap() = Status {
        connected: true,
        profile: Some(name),
        // Initial guess; the `status` command replaces this with the real
        // server-assigned IP (and the live peer list) once the engine reports it.
        assigned_ip: requested_ip,
        mode: Some(mode),
        error: None,
        peers: Vec::new(),
        up_bytes: 0,
        down_bytes: 0,
    };
    Ok(())
}

#[tauri::command]
async fn disconnect(state: tauri::State<'_, AppState>) -> Result<(), String> {
    let handle = { state.engine.lock().unwrap().take() };
    let mut err = None;
    if let Some(h) = handle {
        if let Err(e) = h.stop().await {
            err = Some(e.to_string());
        }
    }
    let mut s = state.status.lock().unwrap();
    s.connected = false;
    s.error = err;
    Ok(())
}

#[tauri::command]
async fn ping_server(
    state: tauri::State<'_, AppState>,
    profile: String,
    server: String,
) -> Result<u64, String> {
    let p = load_profiles(&state.profiles_path)
        .into_iter()
        .find(|p| p.name == profile)
        .ok_or_else(|| format!("profile '{profile}' not found"))?;
    let entry = p
        .servers
        .iter()
        .find(|s| s.name == server)
        .cloned()
        .or_else(|| p.active_server().ok())
        .ok_or_else(|| format!("server '{server}' not found"))?;
    let rtt =
        entrotunnel_client::latency::measure_latency(&entry, std::time::Duration::from_secs(5))
            .await
            .map_err(|e| e.to_string())?;
    Ok(rtt.as_millis() as u64)
}

#[tauri::command]
fn status(state: tauri::State<AppState>) -> Status {
    // Read engine-side info first and drop the engine lock before locking status,
    // so we never hold both at once (all other paths lock engine before status —
    // keeping one global order avoids any lock-inversion deadlock).
    let (alive, assigned_ip, peers, up_bytes, down_bytes) = {
        let eng = state.engine.lock().unwrap();
        match eng.as_ref() {
            Some(h) => {
                let alive = !h.task.is_finished();
                // Live info the engine reports back: the real server-assigned IP
                // and the current VPN peer list (only set when a VPN member).
                let (ip, peers, up, down) = h
                    .shared
                    .lock()
                    .map(|live| {
                        let ip = live.assigned_ip.map(|ip| ip.to_string());
                        let peers = live
                            .peers
                            .iter()
                            .map(|p| Peer {
                                ip: p.ip.to_string(),
                                name: p.name.clone(),
                            })
                            .collect::<Vec<_>>();
                        (ip, peers, live.up_bytes, live.down_bytes)
                    })
                    .unwrap_or((None, Vec::new(), 0, 0));
                (alive, ip, peers, up, down)
            }
            None => (false, None, Vec::new(), 0, 0),
        }
    };

    let mut s = state.status.lock().unwrap().clone();
    s.connected = alive;
    if assigned_ip.is_some() {
        s.assigned_ip = assigned_ip;
    }
    s.peers = peers;
    s.up_bytes = up_bytes;
    s.down_bytes = down_bytes;
    s
}

/// The captured tracing log lines (oldest first) for the Logs page.
#[tauri::command]
fn get_logs(state: tauri::State<AppState>) -> Vec<String> {
    state
        .logs
        .lock()
        .map(|q| q.iter().cloned().collect())
        .unwrap_or_default()
}

/// Bring the main window back to the foreground (from the tray).
fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

/// The only real exit. Flag it (so the close/exit handlers stop intercepting),
/// stop the engine so its TUN/route/system-proxy guards restore the OS state,
/// then exit the process.
fn quit_app(app: &AppHandle) {
    app.state::<AppState>()
        .quitting
        .store(true, Ordering::SeqCst);
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let handle = { app.state::<AppState>().engine.lock().unwrap().take() };
        if let Some(h) = handle {
            let _ = h.stop().await;
        }
        app.exit(0);
    });
}

/// Build the tray icon + menu (Show / Quit) and its event handlers.
fn setup_tray(app: &AppHandle) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "Show EntroTunnel", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit EntroTunnel", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &quit])?;

    TrayIconBuilder::with_id("main")
        .icon(tauri::include_image!("icons/128x128.png"))
        .tooltip("EntroTunnel")
        .menu(&menu)
        // Left-click reopens the window; the menu (incl. Quit) is the right-click.
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => show_main(app),
            "quit" => quit_app(app),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

fn main() {
    // Capture logs into a ring buffer (for the Logs page) while still printing to
    // stderr. `with_ansi(false)` keeps the captured lines free of color codes.
    let logs: SharedLogs = Arc::new(Mutex::new(VecDeque::with_capacity(LOG_CAP)));
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_ansi(false)
        .with_writer(LogTee { buf: logs.clone() })
        .init();

    tauri::Builder::default()
        .setup(move |app| {
            // Honor `--config-dir <path>` (passed when we relaunch elevated) so the
            // root/admin instance still reads the user's profiles; else the OS dir.
            let args: Vec<String> = std::env::args().collect();
            let dir = args
                .iter()
                .position(|a| a == "--config-dir")
                .and_then(|i| args.get(i + 1))
                .map(PathBuf::from)
                .or_else(|| app.path().app_config_dir().ok())
                .unwrap_or_else(|| PathBuf::from("."));
            app.manage(AppState {
                config_dir: dir.clone(),
                profiles_path: dir.join("profiles.json"),
                settings_path: dir.join("settings.json"),
                engine: Mutex::new(None),
                status: Mutex::new(Status::default()),
                logs: logs.clone(),
                quitting: AtomicBool::new(false),
            });
            setup_tray(app.handle())?;
            Ok(())
        })
        // Closing the window hides it to the tray instead of quitting; only the
        // tray's "Quit" (which sets `quitting`) actually exits.
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                if !window
                    .app_handle()
                    .state::<AppState>()
                    .quitting
                    .load(Ordering::SeqCst)
                {
                    let _ = window.hide();
                    api.prevent_close();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            list_profiles,
            upsert_profile,
            remove_profile,
            import_profile,
            export_profile,
            export_profile_toml,
            export_all_profiles,
            import_profiles_toml,
            get_state,
            set_settings,
            set_active_profile,
            gen_psk,
            is_elevated,
            relaunch_elevated,
            connect_profile,
            disconnect,
            status,
            ping_server,
            get_logs,
        ])
        .build(tauri::generate_context!())
        .expect("error while building EntroTunnel")
        .run(|app, event| match event {
            // Keep running in the tray when the OS/user asks to exit (e.g. Cmd-Q
            // or last window closed) — real exit only comes from the tray Quit.
            tauri::RunEvent::ExitRequested { api, .. } => {
                if !app.state::<AppState>().quitting.load(Ordering::SeqCst) {
                    api.prevent_exit();
                }
            }
            // macOS: clicking the Dock icon (with the window hidden to the tray)
            // fires Reopen — bring the window back.
            #[cfg(target_os = "macos")]
            tauri::RunEvent::Reopen { .. } => show_main(app),
            _ => {}
        });
}
