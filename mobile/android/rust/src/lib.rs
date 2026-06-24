//! JNI entry points for the EntroTunnel Android core.
//!
//! Kotlin side: `object com.entrotunnel.android.core.Native` with
//! `external fun nativeStart/nativeStop/nativeStatus/nativeLogs`. The library is
//! `libentrotunnel_jni.so` (System.loadLibrary("entrotunnel_jni")).
//!
//! `nativeStart(profileJson, settingsJson, tunFd)` composes the same
//! `ClientConfig` the desktop uses (`Profile` + `ConnectionSettings`), spawns the
//! engine on a background Tokio runtime, and returns "" on success or an error
//! string. Status/logs are polled.

mod engine;

use entrotunnel_client::config::{ClientConfig, ConnectionSettings, Profile};
use entrotunnel_client::engine::SharedStatus;
use jni::objects::{JClass, JString};
use jni::sys::{jint, jstring};
use jni::JNIEnv;
use once_cell::sync::Lazy;
use std::collections::VecDeque;
use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, Once};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

const LOG_CAP: usize = 2000;

static LOGS: Lazy<Mutex<VecDeque<String>>> = Lazy::new(|| Mutex::new(VecDeque::new()));
static ENGINE: Lazy<Mutex<Option<EngineHandle>>> = Lazy::new(|| Mutex::new(None));
static LAST_ERROR: Lazy<Mutex<Option<String>>> = Lazy::new(|| Mutex::new(None));
/// Delivers the VpnService fd to the connected-but-waiting engine task (packet
/// modes). Set by `nativeConnect`, consumed by `nativeStartBridge`.
static FD_TX: Lazy<Mutex<Option<oneshot::Sender<RawFd>>>> = Lazy::new(|| Mutex::new(None));
static RUNNING: AtomicBool = AtomicBool::new(false);
static LOG_INIT: Once = Once::new();

struct EngineHandle {
    cancel: CancellationToken,
    shared: SharedStatus,
    rt: tokio::runtime::Runtime,
}

// ---- logging: capture tracing output into a ring buffer for the Logs screen --

struct LogMaker;
struct LogWriter(Vec<u8>);

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogMaker {
    type Writer = LogWriter;
    fn make_writer(&'a self) -> LogWriter {
        LogWriter(Vec::new())
    }
}
impl std::io::Write for LogWriter {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl Drop for LogWriter {
    fn drop(&mut self) {
        if self.0.is_empty() {
            return;
        }
        let text = String::from_utf8_lossy(&self.0);
        if let Ok(mut q) = LOGS.lock() {
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

fn init_logging() {
    LOG_INIT.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new(
                "info,entrotunnel_client=info,entrotunnel_core=info,entrotunnel_jni=info",
            ))
            .with_ansi(false)
            .with_writer(LogMaker)
            .try_init();
    });
}

// ---- start / stop -----------------------------------------------------------

fn stop_impl() {
    *FD_TX.lock().unwrap() = None;
    if let Some(h) = ENGINE.lock().unwrap().take() {
        h.cancel.cancel();
        RUNNING.store(false, Ordering::SeqCst);
        // Don't block the JNI thread waiting for tasks to wind down.
        h.rt.shutdown_background();
    }
}

/// Phase 1: parse config, connect + handshake, return the network config JSON
/// (always JSON; on failure `{"error": "..."}`). For packet modes the engine task
/// then waits for the fd (see `start_bridge_impl`); for HTTP-proxy it runs at once.
fn connect_impl(env: &mut JNIEnv, profile_json: &JString, settings_json: &JString) -> String {
    init_logging();
    let parse = (|| -> Result<ClientConfig, String> {
        let profile_s: String = env.get_string(profile_json).map_err(|e| e.to_string())?.into();
        let settings_s: String = env.get_string(settings_json).map_err(|e| e.to_string())?.into();
        let profile: Profile =
            serde_json::from_str(&profile_s).map_err(|e| format!("bad profile JSON: {e}"))?;
        let settings: ConnectionSettings =
            serde_json::from_str(&settings_s).map_err(|e| format!("bad settings JSON: {e}"))?;
        Ok(ClientConfig::compose(&profile, &settings))
    })();
    let cfg = match parse {
        Ok(c) => c,
        Err(e) => return serde_json::json!({ "error": e }).to_string(),
    };

    stop_impl(); // replace any existing session

    let cancel = CancellationToken::new();
    let shared = SharedStatus::default();
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => return serde_json::json!({ "error": e.to_string() }).to_string(),
    };

    let (cfg_tx, cfg_rx) = std::sync::mpsc::channel();
    let (fd_tx, fd_rx) = oneshot::channel::<RawFd>();
    *FD_TX.lock().unwrap() = Some(fd_tx);
    LAST_ERROR.lock().unwrap().take();
    RUNNING.store(true, Ordering::SeqCst);

    let cancel_task = cancel.clone();
    let shared_task = shared.clone();
    rt.spawn(async move {
        if let Err(e) = engine::run(cfg, cfg_tx, fd_rx, cancel_task, shared_task).await {
            tracing::error!("engine exited: {e}");
            *LAST_ERROR.lock().unwrap() = Some(e.to_string());
        }
        RUNNING.store(false, Ordering::SeqCst);
    });
    *ENGINE.lock().unwrap() = Some(EngineHandle { cancel, shared, rt });

    // Block (briefly) for the handshake result / network config.
    match cfg_rx.recv_timeout(std::time::Duration::from_secs(20)) {
        Ok(Ok(v)) => v.to_string(),
        Ok(Err(e)) => {
            stop_impl();
            serde_json::json!({ "error": e }).to_string()
        }
        Err(_) => {
            stop_impl();
            serde_json::json!({ "error": "timed out connecting" }).to_string()
        }
    }
}

/// Phase 2 (packet modes): hand the established VpnService fd to the engine.
fn start_bridge_impl(fd: jint) -> Result<(), String> {
    match FD_TX.lock().unwrap().take() {
        Some(tx) => tx.send(fd as RawFd).map_err(|_| "engine is not awaiting a fd".to_string()),
        None => Err("no pending connection".into()),
    }
}

fn status_json() -> String {
    let running = RUNNING.load(Ordering::SeqCst);
    let (assigned_ip, peers) = ENGINE
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|h| h.shared.lock().ok().map(|s| {
            (
                s.assigned_ip.map(|ip| ip.to_string()),
                s.peers
                    .iter()
                    .map(|p| serde_json::json!({ "ip": p.ip.to_string(), "name": p.name }))
                    .collect::<Vec<_>>(),
            )
        }))
        .unwrap_or((None, Vec::new()));
    let error = LAST_ERROR.lock().unwrap().clone();
    serde_json::json!({
        "running": running,
        "assigned_ip": assigned_ip,
        "peers": peers,
        "error": error,
    })
    .to_string()
}

fn logs_text() -> String {
    LOGS.lock()
        .map(|q| q.iter().cloned().collect::<Vec<_>>().join("\n"))
        .unwrap_or_default()
}

// ---- JNI exports (package com.entrotunnel.android.core, class Native) --------

/// Connect + handshake. Returns the network-config JSON (`mode`, `assigned_ip`,
/// `prefix_len`, `gateway`, `mtu`, `dns`) or `{"error": "..."}`.
#[no_mangle]
pub extern "system" fn Java_com_entrotunnel_android_core_Native_nativeConnect<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    profile_json: JString<'local>,
    settings_json: JString<'local>,
) -> jstring {
    let out = connect_impl(&mut env, &profile_json, &settings_json);
    env.new_string(out)
        .map(|s| s.into_raw())
        .unwrap_or(std::ptr::null_mut())
}

/// Hand the established VpnService fd to the waiting engine (packet modes).
/// Returns "" on success or an error string.
#[no_mangle]
pub extern "system" fn Java_com_entrotunnel_android_core_Native_nativeStartBridge<'local>(
    env: JNIEnv<'local>,
    _class: JClass<'local>,
    tun_fd: jint,
) -> jstring {
    let msg = start_bridge_impl(tun_fd).err().unwrap_or_default();
    env.new_string(msg)
        .map(|s| s.into_raw())
        .unwrap_or(std::ptr::null_mut())
}

#[no_mangle]
pub extern "system" fn Java_com_entrotunnel_android_core_Native_nativeStop<'local>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
) {
    stop_impl();
}

#[no_mangle]
pub extern "system" fn Java_com_entrotunnel_android_core_Native_nativeStatus<'local>(
    env: JNIEnv<'local>,
    _class: JClass<'local>,
) -> jstring {
    env.new_string(status_json())
        .map(|s| s.into_raw())
        .unwrap_or(std::ptr::null_mut())
}

#[no_mangle]
pub extern "system" fn Java_com_entrotunnel_android_core_Native_nativeLogs<'local>(
    env: JNIEnv<'local>,
    _class: JClass<'local>,
) -> jstring {
    env.new_string(logs_text())
        .map(|s| s.into_raw())
        .unwrap_or(std::ptr::null_mut())
}
