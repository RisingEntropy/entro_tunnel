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
            run_packet_mode(
                &cfg, welcome, writer, reader, server_ip, member, shared, cancel,
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
            run_packet_mode(
                &cfg,
                welcome,
                writer,
                reader,
                first_hop_ip,
                false,
                shared,
                cancel,
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

/// Packet path: bridge the TUN device and the encrypted frame channel.
async fn run_packet_mode(
    cfg: &ClientConfig,
    welcome: Welcome,
    mut writer: FrameWriter,
    mut reader: FrameReader,
    server_ip: IpAddr,
    member: bool,
    shared: SharedStatus,
    cancel: CancellationToken,
) -> Result<()> {
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

    // Restored on drop.
    let _net_guard = netcfg::apply(cfg, &welcome, tun.name(), server_ip).await?;

    let read_buf_len = (welcome.mtu as usize).max(2048);

    // Uplink: TUN → server (with keepalive).
    let tun_up = tun.clone();
    let cancel_up = cancel.clone();
    let shared_up = shared.clone();
    let uplink = tokio::spawn(async move {
        let mut buf = vec![0u8; read_buf_len];
        let mut keepalive = tokio::time::interval(Duration::from_secs(KEEPALIVE_SECS));
        keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Poll the server for the VPN peer list (first tick fires immediately so
        // the UI populates right after connecting). Disabled for non-members via
        // the branch precondition below.
        let mut peers_iv = tokio::time::interval(Duration::from_secs(PEER_REFRESH_SECS));
        peers_iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = cancel_up.cancelled() => break,
                _ = keepalive.tick() => {
                    if writer.send(&Frame::Ping).await.is_err() { break; }
                }
                _ = peers_iv.tick(), if member => {
                    if writer.send(&Frame::GetPeers).await.is_err() { break; }
                }
                r = tun_up.recv(&mut buf) => match r {
                    Ok(n) if n > 0 => {
                        if writer.send(&Frame::Packet(buf[..n].to_vec())).await.is_err() { break; }
                        add_traffic_up(&shared_up, n);
                    }
                    Ok(_) => {}
                    Err(e) => { tracing::debug!("tun recv: {e}"); break; }
                }
            }
        }
        let _ = writer.close().await;
    });

    // Downlink: server → TUN (+ cache any VPN peer list the server pushes back).
    let tun_down = tun.clone();
    let cancel_down = cancel.clone();
    let shared_down = shared.clone();
    let downlink = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel_down.cancelled() => break,
                f = reader.recv() => match f {
                    Ok(Frame::Packet(pkt)) => {
                        let n = pkt.len();
                        let _ = tun_down.send(&pkt).await;
                        add_traffic_down(&shared_down, n);
                    }
                    Ok(Frame::Ping | Frame::Pong) => {}
                    Ok(Frame::PeerList { peers }) => {
                        if let Ok(mut s) = shared_down.lock() { s.peers = peers; }
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        }
    });

    tokio::select! {
        _ = cancel.cancelled() => {}
        _ = uplink => {}
        _ = downlink => {}
    }
    cancel.cancel();
    info!("session closed; restoring network configuration");
    Ok(())
}
