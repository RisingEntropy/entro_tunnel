//! The client engine: connect → handshake → run the selected mode.
//!
//! Used identically by the CLI and the Tauri GUI.

use crate::config::{ClientConfig, ServerEntry};
use crate::netcfg;
use crate::tun::{TunConfig, TunDevice};
use entrotunnel_core::config::{parse_psk, SessionMode};
use entrotunnel_core::protocol::{self, Frame, FrameReader, FrameWriter, Hello, PeerInfo, Welcome};
use entrotunnel_core::transport::{self, ClientSecurity, Endpoint};
use entrotunnel_core::{Error, Result, KEEPALIVE_SECS, PROTOCOL_VERSION};
use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::info;

/// How often a VPN client asks the server for the current peer list.
pub(crate) const PEER_REFRESH_SECS: u64 = 5;

/// Reconnect attempts after an unexpected drop before giving up. No delay
/// between them — we re-dial immediately.
const RECONNECT_ATTEMPTS: u32 = 3;
/// Bounded TUN-egress buffer (packets) held across a reconnect: traffic produced
/// while the tunnel is down waits here and is flushed on reconnect. If the buffer
/// fills (long outage) further packets apply backpressure; on give-up it's dropped.
const OUT_BUFFER_PACKETS: usize = 8192;

/// A single frame write that takes longer than this means the socket is wedged
/// (half-open / stalled send window with no RST) — treat the connection as dead.
const SEND_TIMEOUT_SECS: u64 = 8;
/// Traffic-based stall detection (no pings): if ONE direction keeps moving traffic
/// while the OTHER stays completely silent for this many seconds, the tunnel is
/// half-open → reconnect. Both directions silent = idle (nothing to proxy), which
/// never trips. Bump this if normal request/response latency causes false trips.
const STALL_SILENT_SECS: u64 = 1;

/// Live, mutable session info the front-end polls while the engine runs in the
/// background. Written by the engine task, read by the GUI's `status` command.
#[derive(Default)]
pub struct LiveStatus {
    /// The virtual IP the server actually assigned (known once Welcome arrives).
    pub assigned_ip: Option<Ipv4Addr>,
    /// VPN peers currently on the server (refreshed periodically; VPN mode only).
    pub peers: Vec<PeerInfo>,
    /// Payload bytes sent from this client toward the tunnel/server.
    pub up_bytes: u64,
    /// Payload bytes received from the tunnel/server by this client.
    pub down_bytes: u64,
}

/// Shared handle to [`LiveStatus`]. A plain `std::sync::Mutex` is fine: every
/// lock is a brief field read/write with no `.await` held across it.
pub type SharedStatus = Arc<Mutex<LiveStatus>>;

#[inline]
pub(crate) fn add_traffic_up(shared: &SharedStatus, n: usize) {
    if n == 0 {
        return;
    }
    if let Ok(mut s) = shared.lock() {
        s.up_bytes = s.up_bytes.saturating_add(n as u64);
    }
}

#[inline]
pub(crate) fn add_traffic_down(shared: &SharedStatus, n: usize) {
    if n == 0 {
        return;
    }
    if let Ok(mut s) = shared.lock() {
        s.down_bytes = s.down_bytes.saturating_add(n as u64);
    }
}

/// Snapshot of `(up_bytes, down_bytes)` for the stall monitor.
#[inline]
fn traffic_counters(shared: &SharedStatus) -> (u64, u64) {
    shared.lock().map(|s| (s.up_bytes, s.down_bytes)).unwrap_or((0, 0))
}

/// Stateless entry point for running the client.
pub struct Engine;

/// Handle to a background-running engine (used by the GUI).
pub struct EngineHandle {
    pub cancel: CancellationToken,
    pub task: tokio::task::JoinHandle<Result<()>>,
    /// Live session info (assigned IP, VPN peers) the GUI can poll.
    pub shared: SharedStatus,
}

impl EngineHandle {
    /// Signal shutdown and await the engine task.
    pub async fn stop(self) -> Result<()> {
        self.cancel.cancel();
        match self.task.await {
            Ok(r) => r,
            Err(e) => Err(Error::Transport(format!("engine task join: {e}"))),
        }
    }
}

impl Engine {
    /// Run until `cancel` fires or the session ends. Blocks the caller.
    pub async fn run(cfg: ClientConfig, cancel: CancellationToken) -> Result<()> {
        // The CLI has no status poller, so it just discards the shared state.
        run_session(cfg, cancel, SharedStatus::default()).await
    }

    /// Spawn the engine in the background and return a handle.
    pub fn start(cfg: ClientConfig) -> EngineHandle {
        let cancel = CancellationToken::new();
        let token = cancel.clone();
        let shared = SharedStatus::default();
        let shared_task = shared.clone();
        let task = tokio::spawn(async move { run_session(cfg, token, shared_task).await });
        EngineHandle {
            cancel,
            task,
            shared,
        }
    }
}

async fn resolve_server(host: &str, port: u16) -> Result<IpAddr> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(ip);
    }
    let mut addrs = tokio::net::lookup_host((host, port)).await?;
    addrs
        .next()
        .map(|s| s.ip())
        .ok_or_else(|| Error::Config(format!("cannot resolve host {host}")))
}

async fn run_session(
    cfg: ClientConfig,
    cancel: CancellationToken,
    shared: SharedStatus,
) -> Result<()> {
    // Proxy chain: when configured with ≥2 valid hops, relay through them.
    if !cfg.chain.is_empty() {
        let hops = crate::chain::resolve_hops(&cfg.chain, &cfg.servers);
        if hops.len() >= 2 {
            return run_chain_session(cfg, hops, cancel, shared).await;
        }
    }

    // Pick the selected server (or fall back to a legacy single-server config).
    let server = cfg.active_server()?;
    let server_ip = resolve_server(&server.host, server.port).await?;

    let endpoint = Endpoint {
        host: server.host.clone(),
        port: server.port,
        kind: server.transport,
        ws_path: "/et".to_string(),
        server_name: server.sni(),
    };
    let security = ClientSecurity {
        noise_psk: parse_psk(&server.noise_psk)?,
        tls_skip_verify: server.tls_skip_verify,
        tls_pinned_cert_pem: None,
    };

    info!(
        server = %format!("{} ({}:{})", server.name, server.host, server.port),
        transport = %server.transport,
        mode = %cfg.mode,
        "connecting"
    );
    let (sink, stream) = transport::connect(&endpoint, &security).await?;
    let (mut writer, mut reader) = protocol::frames(sink, stream);

    let hello = Hello {
        version: PROTOCOL_VERSION,
        token: server.token.clone(),
        // System-proxy is a client-side concern; on the wire it is HTTP-proxy,
        // so existing servers need no awareness of the new mode.
        mode: cfg.mode.wire(),
        requested_ip: cfg.requested_ip,
        client_name: cfg.client_name.clone(),
        join_vpn: cfg.join_vpn,
    };
    writer.send(&Frame::Hello(hello)).await?;

    let welcome = match reader.recv().await? {
        Frame::Welcome(w) => w,
        Frame::Reject { reason } => return Err(Error::Auth(reason)),
        other => return Err(Error::Protocol(format!("expected Welcome, got {other:?}"))),
    };
    info!(
        ip = %welcome.assigned_ip,
        prefix = welcome.prefix_len,
        gateway = %welcome.gateway,
        mtu = welcome.mtu,
        "session established"
    );
    // Report the server-assigned IP to the front-end (Ipv4Addr is Copy, so this
    // does not disturb `welcome`, which packet mode consumes below).
    if let Ok(mut s) = shared.lock() {
        s.assigned_ip = Some(welcome.assigned_ip);
    }

    // A "VPN member" participates in the peer LAN: native `Vpn` mode, or any mode
    // with `join_vpn`. Members poll the server for the peer list and (in a proxy
    // mode) bring up a TUN routed at just the virtual subnet.
    let member = matches!(cfg.mode, SessionMode::Vpn) || cfg.join_vpn;
    match cfg.mode {
        SessionMode::GlobalProxy | SessionMode::Vpn => {
            // Reconnect dials the server's *resolved IP* (DNS is tunnelled while
            // we're down) with the original SNI, re-handshaking for the same
            // virtual IP so the existing routing/DNS stay valid.
            let re_endpoint = Endpoint {
                host: server_ip.to_string(),
                port: server.port,
                kind: server.transport,
                ws_path: "/et".to_string(),
                server_name: server.sni(),
            };
            let re_security = security.clone();
            let re_token = server.token.clone();
            let re_mode = cfg.mode.wire();
            let re_name = cfg.client_name.clone();
            let re_join = cfg.join_vpn;
            let reconnect = move |req_ip: Ipv4Addr| {
                let endpoint = re_endpoint.clone();
                let security = re_security.clone();
                let token = re_token.clone();
                let mode = re_mode; // SessionMode is Copy
                let client_name = re_name.clone();
                let join_vpn = re_join;
                async move {
                    let (sink, stream) = transport::connect(&endpoint, &security).await?;
                    let (mut w, mut r) = protocol::frames(sink, stream);
                    let hello = Hello {
                        version: PROTOCOL_VERSION,
                        token,
                        mode,
                        requested_ip: Some(req_ip),
                        client_name,
                        join_vpn,
                    };
                    w.send(&Frame::Hello(hello)).await?;
                    let welcome = match r.recv().await? {
                        Frame::Welcome(wl) => wl,
                        Frame::Reject { reason } => return Err(Error::Auth(reason)),
                        other => return Err(Error::Protocol(format!("expected Welcome, got {other:?}"))),
                    };
                    Ok((w, r, welcome))
                }
            };
            run_packet_mode(
                &cfg, welcome, writer, reader, server_ip, member, shared, cancel, reconnect,
            )
            .await
        }
        SessionMode::HttpProxy => {
            crate::proxy::run_http_mode(&cfg, welcome, writer, reader, member, shared, cancel).await
        }
        SessionMode::SystemProxy => {
            // Same local proxy as HTTP-proxy mode, but also flip the OS proxy
            // switch for its lifetime (restored when the guard drops).
            let _sysproxy = crate::sysproxy::enable(&cfg.http_listen);
            crate::proxy::run_http_mode(&cfg, welcome, writer, reader, member, shared, cancel).await
        }
    }
}

/// Egress (and optional VPN-LAN) over a multi-hop proxy chain.
async fn run_chain_session(
    cfg: ClientConfig,
    hops: Vec<ServerEntry>,
    cancel: CancellationToken,
    shared: SharedStatus,
) -> Result<()> {
    let final_mode = cfg.mode;

    // Pure VPN over a chain just means "join the FIRST hop's LAN" — there is no
    // internet egress to relay, so connect straight to hop 0 in VPN mode.
    if matches!(final_mode, SessionMode::Vpn) {
        let mut c = cfg;
        c.chain = Vec::new();
        c.selected_server = Some(hops[0].name.clone());
        return Box::pin(run_session(c, cancel, shared)).await;
    }

    // Pre-resolve the FIRST hop's IP now — while DNS still works, before the
    // tunnel captures it. Dialing hop 0 by IP lets reconnects succeed even when
    // DNS is down during an outage (DNS routes through the stalled tunnel, so
    // re-resolving a hostname there fails — the cause of chains never recovering).
    // The original hostname is preserved as the TLS SNI. Later hops are resolved
    // server-side by the relay, so they don't need this.
    let hops = {
        let mut hops = hops;
        match resolve_server(&hops[0].host, hops[0].port).await {
            Ok(ip) => {
                if hops[0].server_name.is_none() && hops[0].host.parse::<IpAddr>().is_err() {
                    hops[0].server_name = Some(hops[0].host.clone());
                }
                hops[0].host = ip.to_string();
            }
            Err(e) => tracing::warn!(
                "could not pre-resolve first chain hop {}: {e}; reconnect will fail if DNS goes down",
                hops[0].host
            ),
        }
        hops
    };

    // Egress: build the chain; the chosen mode runs at the last hop.
    let chain = crate::chain::connect_chain(
        &hops,
        final_mode,
        cfg.requested_ip,
        cfg.client_name.clone(),
        &cancel,
    )
    .await?;
    let crate::chain::ChainTunnel {
        writer,
        reader,
        welcome,
        first_hop_ip,
    } = chain;

    // When also joining the VPN, the LAN is the FIRST hop (a separate direct
    // connection that owns the displayed virtual IP + peer list). Otherwise the
    // egress tunnel's assigned IP is reported.
    if cfg.join_vpn {
        spawn_vpn_lan(&cfg, &hops[0], cancel.clone(), shared.clone());
    } else if let Ok(mut s) = shared.lock() {
        s.assigned_ip = Some(welcome.assigned_ip);
    }

    // The egress tunnel never brings up the peer LAN itself (member = false); that
    // is the first-hop connection's job above.
    match final_mode {
        SessionMode::GlobalProxy => {
            // Reconnect rebuilds the whole chain, re-handshaking the egress hop
            // for the same virtual IP so routing stays valid.
            let re_hops = hops.clone();
            let re_name = cfg.client_name.clone();
            let re_cancel = cancel.clone();
            let reconnect = move |req_ip: Ipv4Addr| {
                let hops = re_hops.clone();
                let client_name = re_name.clone();
                let cancel = re_cancel.clone();
                async move {
                    let ch = crate::chain::connect_chain(
                        &hops,
                        SessionMode::GlobalProxy,
                        Some(req_ip),
                        client_name,
                        &cancel,
                    )
                    .await?;
                    Ok((ch.writer, ch.reader, ch.welcome))
                }
            };
            run_packet_mode(
                &cfg,
                welcome,
                writer,
                reader,
                first_hop_ip,
                false,
                shared,
                cancel,
                reconnect,
            )
            .await
        }
        SessionMode::HttpProxy => {
            crate::proxy::run_http_mode(&cfg, welcome, writer, reader, false, shared, cancel).await
        }
        SessionMode::SystemProxy => {
            let _sysproxy = crate::sysproxy::enable(&cfg.http_listen);
            crate::proxy::run_http_mode(&cfg, welcome, writer, reader, false, shared, cancel).await
        }
        SessionMode::Vpn => unreachable!("handled above"),
    }
}

/// Spawn a separate direct VPN connection to `hop0` so the client joins its peer
/// LAN while egress flows through the chain. Uses a distinct TUN name so it does
/// not collide with a global-proxy egress TUN.
fn spawn_vpn_lan(
    cfg: &ClientConfig,
    hop0: &ServerEntry,
    cancel: CancellationToken,
    shared: SharedStatus,
) {
    let mut c = cfg.clone();
    c.mode = SessionMode::Vpn;
    c.join_vpn = false;
    c.chain = Vec::new();
    c.selected_server = Some(hop0.name.clone());
    c.tun_name = format!("{}v", cfg.tun_name);
    tokio::spawn(async move {
        if let Err(e) = run_session(c, cancel, shared).await {
            tracing::warn!("chain VPN LAN (first hop) ended: {e}");
        }
    });
}

/// Outcome of bridging one transport connection.
enum ConnEnd {
    /// The user cancelled (or the TUN reader ended) — shut down for good.
    Cancelled,
    /// The transport connection dropped — try to reconnect.
    Lost,
}

/// Bridge the TUN and the current encrypted channel until it drops or is
/// cancelled. Reads egress packets from the persistent `out_rx` buffer (so
/// packets queued during a reconnect are flushed here on the next connection).
async fn bridge_connection(
    writer: &mut FrameWriter,
    reader: &mut FrameReader,
    out_rx: &mut tokio::sync::mpsc::Receiver<Vec<u8>>,
    tun: &Arc<TunDevice>,
    member: bool,
    shared: &SharedStatus,
    cancel: &CancellationToken,
) -> ConnEnd {
    // Uplink and downlink run CONCURRENTLY (two futures polled together), NOT
    // merged into one select loop: a slow `writer.send()` must never stall
    // `reader.recv()`, or the socket's receive buffer backs up and throughput
    // collapses — acute on high-latency multi-hop chains. Uplink returns `true`
    // if the writer died (reconnectable) or `false` if the egress buffer closed
    // (TUN reader gone → fatal). Downlink returns on any reader error.
    //
    // Stall detection is TRAFFIC-based (no pings): the `monitor` future watches
    // the byte counters and reconnects the moment one direction goes silent while
    // the other is active (a half-open tunnel). A low-rate keepalive still runs
    // purely to hold the connection open when genuinely idle.
    let send_to = Duration::from_secs(SEND_TIMEOUT_SECS);

    let uplink = async move {
        let mut keepalive = tokio::time::interval(Duration::from_secs(KEEPALIVE_SECS));
        keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Poll the server for the VPN peer list (first tick fires immediately).
        let mut peers_iv = tokio::time::interval(Duration::from_secs(PEER_REFRESH_SECS));
        peers_iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = keepalive.tick() => {
                    match tokio::time::timeout(send_to, writer.send(&Frame::Ping)).await {
                        Ok(Ok(())) => {}
                        _ => return true,
                    }
                }
                _ = peers_iv.tick(), if member => {
                    match tokio::time::timeout(send_to, writer.send(&Frame::GetPeers)).await {
                        Ok(Ok(())) => {}
                        _ => return true,
                    }
                }
                pkt = out_rx.recv() => match pkt {
                    Some(p) => {
                        let n = p.len();
                        match tokio::time::timeout(send_to, writer.send(&Frame::Packet(p))).await {
                            Ok(Ok(())) => add_traffic_up(shared, n),
                            _ => return true,
                        }
                    }
                    None => return false, // TUN reader ended (cancel / fatal)
                }
            }
        }
    };
    let downlink = async move {
        loop {
            match reader.recv().await {
                Ok(Frame::Packet(pkt)) => {
                    let n = pkt.len();
                    let _ = tun.send(&pkt).await;
                    add_traffic_down(shared, n);
                }
                Ok(Frame::PeerList { peers }) => {
                    if let Ok(mut s) = shared.lock() { s.peers = peers; }
                }
                Ok(_) => {}
                Err(_) => return,
            }
        }
    };
    // Traffic monitor: each second, if exactly ONE direction moved bytes (the
    // other silent) for STALL_SILENT_SECS running, the tunnel is half-open →
    // reconnect. Both moving = healthy; both silent = idle (no trip). Keepalive
    // Ping/Pong traffic is negligible and only registers on idle seconds where
    // both sides move together, so it never causes a false trip.
    let monitor = async {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        tick.tick().await; // consume the immediate first tick
        let (mut last_up, mut last_down) = traffic_counters(shared);
        let mut silent_secs = 0u64;
        loop {
            tick.tick().await;
            let (up, down) = traffic_counters(shared);
            let up_moved = up > last_up;
            let down_moved = down > last_down;
            last_up = up;
            last_down = down;
            if up_moved != down_moved {
                silent_secs += 1;
                if silent_secs >= STALL_SILENT_SECS {
                    let dead = if up_moved { "downlink" } else { "uplink" };
                    tracing::warn!(
                        "no {dead} traffic for {}s while {} is active — tunnel stalled; reconnecting",
                        silent_secs,
                        if up_moved { "uplink" } else { "downlink" }
                    );
                    return;
                }
            } else {
                silent_secs = 0; // both moving (healthy) or both idle
            }
        }
    };

    tokio::pin!(uplink, downlink, monitor);
    tokio::select! {
        _ = cancel.cancelled() => ConnEnd::Cancelled,
        writer_died = &mut uplink => if writer_died { ConnEnd::Lost } else { ConnEnd::Cancelled },
        _ = &mut downlink => ConnEnd::Lost,
        _ = &mut monitor => ConnEnd::Lost,
    }
}

/// Packet path: bridge the TUN device and the encrypted frame channel, with
/// automatic reconnect. The TUN device and OS routing/DNS ([`netcfg`]) are
/// brought up ONCE and kept alive across reconnects; only the transport
/// connection is re-established. `reconnect(requested_ip)` re-dials + re-
/// handshakes (asking for the same virtual IP so routing stays valid).
#[allow(clippy::too_many_arguments)]
async fn run_packet_mode<F, Fut>(
    cfg: &ClientConfig,
    welcome: Welcome,
    mut writer: FrameWriter,
    mut reader: FrameReader,
    server_ip: IpAddr,
    member: bool,
    shared: SharedStatus,
    cancel: CancellationToken,
    reconnect: F,
) -> Result<()>
where
    F: Fn(Ipv4Addr) -> Fut + Send,
    Fut: std::future::Future<Output = Result<(FrameWriter, FrameReader, Welcome)>> + Send,
{
    let assigned_ip = welcome.assigned_ip;
    let tun = Arc::new(
        TunDevice::create(&TunConfig {
            name: cfg.tun_name.clone(),
            ip: welcome.assigned_ip,
            prefix_len: welcome.prefix_len,
            mtu: welcome.mtu,
        })
        .await?,
    );
    info!(dev = %tun.name(), "TUN device up");

    // Restored on drop — kept alive across reconnects so routing/DNS persist.
    let _net_guard = netcfg::apply(cfg, &welcome, tun.name(), server_ip).await?;

    let read_buf_len = (welcome.mtu as usize).max(2048);

    // Long-lived TUN → buffer reader. Feeds a bounded channel that PERSISTS
    // across reconnects, so egress produced while the tunnel is down is buffered
    // (and flushed by the next `bridge_connection`) instead of lost. On give-up
    // the channel is dropped and any buffered packets discarded.
    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(OUT_BUFFER_PACKETS);
    let tun_reader = tun.clone();
    let cancel_reader = cancel.clone();
    let reader_task = tokio::spawn(async move {
        let mut buf = vec![0u8; read_buf_len];
        loop {
            let n = tokio::select! {
                _ = cancel_reader.cancelled() => break,
                r = tun_reader.recv(&mut buf) => match r {
                    Ok(n) if n > 0 => n,
                    Ok(_) => continue,
                    Err(e) => { tracing::debug!("tun recv: {e}"); break; }
                }
            };
            let pkt = buf[..n].to_vec();
            tokio::select! {
                _ = cancel_reader.cancelled() => break,
                res = out_tx.send(pkt) => if res.is_err() { break; },
            }
        }
    });

    // Connection loop: bridge the current channel; on an unexpected drop, re-dial
    // immediately (up to RECONNECT_ATTEMPTS) while the TUN + routing stay up.
    loop {
        match bridge_connection(&mut writer, &mut reader, &mut out_rx, &tun, member, &shared, &cancel).await {
            ConnEnd::Cancelled => break,
            ConnEnd::Lost => {
                if cancel.is_cancelled() {
                    break;
                }
                let _ = writer.close().await;
                tracing::warn!("tunnel connection lost; reconnecting (buffering traffic)…");
                let mut reconnected = false;
                for attempt in 1..=RECONNECT_ATTEMPTS {
                    // Race each attempt against cancel so a user disconnect is
                    // prompt even mid-dial.
                    let result = tokio::select! {
                        _ = cancel.cancelled() => break,
                        r = reconnect(assigned_ip) => r,
                    };
                    match result {
                        Ok((w, r, new_welcome)) => {
                            if new_welcome.assigned_ip != assigned_ip {
                                tracing::warn!(
                                    "reconnect assigned a different IP ({} vs {}); routing may be stale",
                                    new_welcome.assigned_ip, assigned_ip
                                );
                            }
                            writer = w;
                            reader = r;
                            if let Ok(mut s) = shared.lock() {
                                s.assigned_ip = Some(new_welcome.assigned_ip);
                            }
                            reconnected = true;
                            info!("reconnected on attempt {attempt}/{RECONNECT_ATTEMPTS}; flushing buffered traffic");
                            break;
                        }
                        Err(e) => tracing::warn!(
                            "reconnect attempt {attempt}/{RECONNECT_ATTEMPTS} failed: {e}"
                        ),
                    }
                }
                if cancel.is_cancelled() {
                    break; // user disconnected during a reconnect attempt
                }
                if !reconnected {
                    tracing::warn!(
                        "reconnect failed after {RECONNECT_ATTEMPTS} attempts; disconnecting (buffered traffic dropped)"
                    );
                    break;
                }
            }
        }
    }

    cancel.cancel();
    reader_task.abort();
    info!("session closed; restoring network configuration");
    Ok(())
}
