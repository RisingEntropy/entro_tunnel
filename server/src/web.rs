//! Web admin panel: a small axum REST API plus an embedded single-file SPA.
//!
//! Auth is a shared bearer token (`web.admin_token`), accepted as `?token=` or
//! `Authorization: Bearer <token>`. Bind to localhost (default) or front it with
//! a reverse proxy for remote access.

use crate::config::PeerConfig;
use crate::session::{AppState, PeerStat, TrafficSample};
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::Html,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;

const ADMIN_HTML: &str = include_str!("../web/admin.html");

pub async fn serve(state: AppState, bind: SocketAddr) -> anyhow::Result<()> {
    let app = Router::new()
        .route("/", get(index))
        .route("/api/status", get(status))
        .route("/api/stats", get(stats))
        .route("/api/peers", post(add_peer))
        .route("/api/peers/:token", axum::routing::delete(del_peer))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!(%bind, "web admin listening (open http://{bind}/ )");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn index() -> Html<&'static str> {
    Html(ADMIN_HTML)
}

#[derive(Deserialize)]
struct Auth {
    #[serde(default)]
    token: String,
}

fn authorized(state: &AppState, q: &Auth, headers: &HeaderMap) -> bool {
    let expected = {
        let cfg = state.config.read().unwrap();
        cfg.web.admin_token.clone()
    };
    if !q.token.is_empty() && q.token == expected {
        return true;
    }
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|t| t == expected)
        .unwrap_or(false)
}

#[derive(Serialize)]
struct ListenerView {
    transport: String,
    bind: String,
    /// Whether this listener terminates TLS itself (self-signed) vs sits behind
    /// a TLS-terminating proxy (`tls=false`). Used to default export verification.
    tls: bool,
}

#[derive(Serialize)]
struct PeerView {
    name: String,
    token: String,
    ip: String,
    enabled: bool,
    allow_global: bool,
    allow_http_proxy: bool,
    online: bool,
    mode: Option<String>,
    addr: Option<String>,
}

#[derive(Serialize)]
struct StatusResp {
    listeners: Vec<ListenerView>,
    subnet: String,
    gateway: String,
    mtu: u16,
    /// Shared Noise PSK (raw-TCP channel auth). Exposed only to the authenticated
    /// admin so the panel can build a peer's importable config link in-browser.
    noise_psk: String,
    peers: Vec<PeerView>,
}

async fn status(
    State(state): State<AppState>,
    Query(auth): Query<Auth>,
    headers: HeaderMap,
) -> Result<Json<StatusResp>, StatusCode> {
    if !authorized(&state, &auth, &headers) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let online: HashMap<_, _> = state.router.online().into_iter().collect();
    let cfg = state.config.read().unwrap();

    let peers = cfg
        .peers
        .iter()
        .map(|p| {
            let o = online.get(&p.ip);
            PeerView {
                name: p.name.clone(),
                token: p.token.clone(),
                ip: p.ip.to_string(),
                enabled: p.enabled,
                allow_global: p.allow_global,
                allow_http_proxy: p.allow_http_proxy,
                online: o.is_some(),
                mode: o.map(|x| x.mode.to_string()),
                addr: o.map(|x| x.addr.clone()),
            }
        })
        .collect();
    let listeners = cfg
        .listeners
        .iter()
        .map(|l| ListenerView {
            transport: l.transport.to_string(),
            bind: l.bind.clone(),
            tls: l.tls,
        })
        .collect();

    Ok(Json(StatusResp {
        listeners,
        subnet: cfg.network.subnet.clone(),
        gateway: cfg.network.gateway.to_string(),
        mtu: cfg.network.mtu,
        noise_psk: cfg.security.noise_psk.clone(),
        peers,
    }))
}

#[derive(Serialize)]
struct StatsResp {
    uptime_secs: u64,
    total_up: u64,
    total_down: u64,
    online: usize,
    timeline: Vec<TrafficSample>,
    peers: Vec<PeerStat>,
}

/// In-memory traffic metrics since server start (per-peer totals, online-by-mode,
/// throughput timeline). Never persisted to disk.
async fn stats(
    State(state): State<AppState>,
    Query(auth): Query<Auth>,
    headers: HeaderMap,
) -> Result<Json<StatsResp>, StatusCode> {
    if !authorized(&state, &auth, &headers) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let live = state.router.live();
    let online = live.len();
    let peers = state.metrics.peer_stats(&live);
    let (total_up, total_down) = state.metrics.totals();
    Ok(Json(StatsResp {
        uptime_secs: state.metrics.uptime_secs(),
        total_up,
        total_down,
        online,
        timeline: state.metrics.timeline(),
        peers,
    }))
}

fn persist(state: &AppState) -> Result<(), StatusCode> {
    let cfg = state.config.read().unwrap();
    cfg.save(&state.config_path)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

async fn add_peer(
    State(state): State<AppState>,
    Query(auth): Query<Auth>,
    headers: HeaderMap,
    Json(peer): Json<PeerConfig>,
) -> Result<StatusCode, StatusCode> {
    if !authorized(&state, &auth, &headers) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    {
        let mut cfg = state.config.write().unwrap();
        if cfg.peers.iter().any(|p| p.token == peer.token || p.ip == peer.ip) {
            return Err(StatusCode::CONFLICT);
        }
        cfg.peers.push(peer);
    }
    persist(&state)?;
    Ok(StatusCode::CREATED)
}

async fn del_peer(
    State(state): State<AppState>,
    Query(auth): Query<Auth>,
    headers: HeaderMap,
    Path(token): Path<String>,
) -> Result<StatusCode, StatusCode> {
    if !authorized(&state, &auth, &headers) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    {
        let mut cfg = state.config.write().unwrap();
        let before = cfg.peers.len();
        cfg.peers.retain(|p| p.token != token);
        if cfg.peers.len() == before {
            return Err(StatusCode::NOT_FOUND);
        }
    }
    persist(&state)?;
    Ok(StatusCode::NO_CONTENT)
}
