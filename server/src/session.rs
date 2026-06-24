//! Per-connection session handling, the virtual-IP router, and the shared
//! server-side TUN reader that switches packets between peers / egress.

use crate::config::ServerConfig;
use dashmap::DashMap;
use entrotunnel_core::config::SessionMode;
use entrotunnel_core::protocol::{self, Frame, PeerInfo, TargetAddr, Welcome};
use entrotunnel_core::transport::Accepted;
use entrotunnel_core::tun::TunDevice;
use entrotunnel_core::PROTOCOL_VERSION;
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use uuid::Uuid;

/// Public info about a connected peer (for the admin API).
#[derive(Debug, Clone, Serialize)]
pub struct Online {
    pub name: String,
    pub mode: SessionMode,
    pub addr: String,
    /// Part of the VPN peer LAN: `mode == Vpn` or the client opted in via
    /// `Hello.join_vpn`. Only members are listed to other clients on `GetPeers`.
    pub vpn_member: bool,
    /// The peer's assigned virtual IPv6, when the server runs dual-stack.
    pub ip6: Option<Ipv6Addr>,
}

/// Per-session byte counters (lock-free; read by the metrics aggregator).
#[derive(Default)]
pub struct Counters {
    pub up: AtomicU64,   // bytes client → server
    pub down: AtomicU64, // bytes server → client
}

struct Entry {
    tx: mpsc::Sender<Frame>,
    info: Online,
    token: String,
    counters: Arc<Counters>,
    connected: Instant,
}

/// One online peer plus its live traffic (snapshot for the metrics API).
pub struct LiveEntry {
    pub ip: Ipv4Addr,
    pub name: String,
    pub token: String,
    pub mode: SessionMode,
    pub addr: String,
    pub up: u64,
    pub down: u64,
    pub connected_secs: u64,
}

/// Maps a virtual IP to the live session that owns it. The v4 map is the source
/// of truth (admin/metrics); `peers6` is a lightweight delivery index so IPv6
/// packets off the TUN can be routed to their owning session by destination.
#[derive(Default)]
pub struct Router {
    peers: DashMap<Ipv4Addr, Entry>,
    peers6: DashMap<Ipv6Addr, mpsc::Sender<Frame>>,
}

impl Router {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn register(
        &self,
        ip: Ipv4Addr,
        ip6: Option<Ipv6Addr>,
        tx: mpsc::Sender<Frame>,
        info: Online,
        token: String,
        counters: Arc<Counters>,
    ) {
        if let Some(v6) = ip6 {
            self.peers6.insert(v6, tx.clone());
        }
        self.peers.insert(
            ip,
            Entry {
                tx,
                info,
                token,
                counters,
                connected: Instant::now(),
            },
        );
    }

    fn unregister(&self, ip: Ipv4Addr, ip6: Option<Ipv6Addr>) {
        self.peers.remove(&ip);
        if let Some(v6) = ip6 {
            self.peers6.remove(&v6);
        }
    }

    /// Try to hand a frame to the session owning `ip`. Returns false if no live
    /// session or its queue is full (packet dropped, as with a real NIC).
    pub fn deliver(&self, ip: Ipv4Addr, frame: Frame) -> bool {
        match self.peers.get(&ip) {
            Some(e) => e.tx.try_send(frame).is_ok(),
            None => false,
        }
    }

    /// Deliver an IPv6 packet to the session owning the destination v6 address.
    pub fn deliver6(&self, ip6: Ipv6Addr, frame: Frame) -> bool {
        match self.peers6.get(&ip6) {
            Some(tx) => tx.try_send(frame).is_ok(),
            None => false,
        }
    }

    pub fn online(&self) -> Vec<(Ipv4Addr, Online)> {
        self.peers
            .iter()
            .map(|e| (*e.key(), e.value().info.clone()))
            .collect()
    }

    /// Snapshot of online peers with their live traffic counters.
    pub fn live(&self) -> Vec<LiveEntry> {
        self.peers
            .iter()
            .map(|e| {
                let v = e.value();
                LiveEntry {
                    ip: *e.key(),
                    name: v.info.name.clone(),
                    token: v.token.clone(),
                    mode: v.info.mode,
                    addr: v.info.addr.clone(),
                    up: v.counters.up.load(Ordering::Relaxed),
                    down: v.counters.down.load(Ordering::Relaxed),
                    connected_secs: v.connected.elapsed().as_secs(),
                }
            })
            .collect()
    }
}

/// Cumulative traffic for one peer that has since disconnected (kept by token so
/// totals survive reconnects within this process's lifetime).
struct PeerAgg {
    name: String,
    ip: Ipv4Addr,
    mode: SessionMode,
    up: u64,
    down: u64,
}

/// One throughput sample: bytes transferred during the interval ending at `t`
/// seconds after server start.
#[derive(Clone, Serialize)]
pub struct TrafficSample {
    pub t: u64,
    pub up: u64,
    pub down: u64,
}

/// Per-peer line in the stats API: cumulative bytes since server start, whether
/// the peer is currently connected, and in which mode.
#[derive(Clone, Serialize)]
pub struct PeerStat {
    pub name: String,
    pub token: String,
    pub ip: String,
    pub mode: Option<String>,
    pub online: bool,
    pub addr: Option<String>,
    pub up: u64,
    pub down: u64,
    pub connected_secs: u64,
}

const MAX_SAMPLES: usize = 180; // ~15 min at a 5s sampling interval

/// In-memory server metrics since process start (never written to disk).
pub struct Metrics {
    start: Instant,
    total_up: AtomicU64,
    total_down: AtomicU64,
    /// Cumulative per-peer totals for *disconnected* sessions, keyed by token.
    agg: DashMap<String, PeerAgg>,
    /// Per-interval throughput ring buffer.
    timeline: Mutex<VecDeque<TrafficSample>>,
    /// Last cumulative (up, down) read by the sampler, to compute deltas.
    last: Mutex<(u64, u64)>,
}

impl Metrics {
    pub fn new() -> Arc<Self> {
        Arc::new(Metrics {
            start: Instant::now(),
            total_up: AtomicU64::new(0),
            total_down: AtomicU64::new(0),
            agg: DashMap::new(),
            timeline: Mutex::new(VecDeque::new()),
            last: Mutex::new((0, 0)),
        })
    }

    pub fn uptime_secs(&self) -> u64 {
        self.start.elapsed().as_secs()
    }

    #[inline]
    fn add_up(&self, n: u64) {
        self.total_up.fetch_add(n, Ordering::Relaxed);
    }
    #[inline]
    fn add_down(&self, n: u64) {
        self.total_down.fetch_add(n, Ordering::Relaxed);
    }

    pub fn totals(&self) -> (u64, u64) {
        (
            self.total_up.load(Ordering::Relaxed),
            self.total_down.load(Ordering::Relaxed),
        )
    }

    /// Fold a finished session's counters into the cumulative per-peer totals.
    fn fold_session(&self, token: &str, name: &str, ip: Ipv4Addr, mode: SessionMode, up: u64, down: u64) {
        let mut e = self.agg.entry(token.to_string()).or_insert(PeerAgg {
            name: name.to_string(),
            ip,
            mode,
            up: 0,
            down: 0,
        });
        e.name = name.to_string();
        e.ip = ip;
        e.mode = mode;
        e.up += up;
        e.down += down;
    }

    /// Append one throughput sample (called periodically by the sampler task).
    pub fn sample(&self) {
        let (cu, cd) = self.totals();
        let mut last = self.last.lock().unwrap();
        let du = cu.saturating_sub(last.0);
        let dd = cd.saturating_sub(last.1);
        *last = (cu, cd);
        drop(last);
        let mut tl = self.timeline.lock().unwrap();
        tl.push_back(TrafficSample {
            t: self.uptime_secs(),
            up: du,
            down: dd,
        });
        while tl.len() > MAX_SAMPLES {
            tl.pop_front();
        }
    }

    pub fn timeline(&self) -> Vec<TrafficSample> {
        self.timeline.lock().unwrap().iter().cloned().collect()
    }

    /// Merge cumulative (disconnected) totals with live online sessions into the
    /// per-peer list for the stats API. Live counters are added on top of any
    /// folded history for the same token — no double counting, since a session
    /// is only folded once, on disconnect.
    pub fn peer_stats(&self, live: &[LiveEntry]) -> Vec<PeerStat> {
        let mut out: HashMap<String, PeerStat> = HashMap::new();
        for e in self.agg.iter() {
            let a = e.value();
            out.insert(
                e.key().clone(),
                PeerStat {
                    name: a.name.clone(),
                    token: e.key().clone(),
                    ip: a.ip.to_string(),
                    mode: Some(a.mode.to_string()),
                    online: false,
                    addr: None,
                    up: a.up,
                    down: a.down,
                    connected_secs: 0,
                },
            );
        }
        for l in live {
            let s = out.entry(l.token.clone()).or_insert(PeerStat {
                name: l.name.clone(),
                token: l.token.clone(),
                ip: l.ip.to_string(),
                mode: None,
                online: false,
                addr: None,
                up: 0,
                down: 0,
                connected_secs: 0,
            });
            s.name = l.name.clone();
            s.ip = l.ip.to_string();
            s.mode = Some(l.mode.to_string());
            s.online = true;
            s.addr = Some(l.addr.clone());
            s.up += l.up;
            s.down += l.down;
            s.connected_secs = l.connected_secs;
        }
        let mut v: Vec<PeerStat> = out.into_values().collect();
        v.sort_by(|a, b| (b.up + b.down).cmp(&(a.up + a.down)));
        v
    }
}

/// Shared state passed to connection handlers and the web admin.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<RwLock<ServerConfig>>,
    pub config_path: Arc<PathBuf>,
    pub router: Arc<Router>,
    pub tun: Option<Arc<TunDevice>>,
    pub metrics: Arc<Metrics>,
    /// True when the server set up IPv6 NAT66 (so it advertises v6 to clients).
    pub ipv6: bool,
}

/// Derive a client's virtual IPv6 from the ULA subnet and its pinned v4 address.
/// The (unique-per-peer) v4 goes in the low 32 bits, so it's stable and never
/// collides: `10.66.0.2` in `fd66::/64` → `fd66::a42:2`.
fn derive_ipv6(subnet6: &ipnet::Ipv6Net, v4: Ipv4Addr) -> Ipv6Addr {
    let mut o = subnet6.network().octets();
    o[12..16].copy_from_slice(&u32::from(v4).to_be_bytes());
    Ipv6Addr::from(o)
}

/// Read packets off the server TUN and route them to the owning peer session.
/// The Linux kernel does the actual NAT/forwarding; this just demultiplexes
/// by destination IP (peer-to-peer VPN traffic and egress replies alike).
pub async fn run_tun_router(tun: Arc<TunDevice>, router: Arc<Router>) {
    let mut buf = vec![0u8; 65535];
    loop {
        match tun.recv(&mut buf).await {
            Ok(n) if n >= 20 => {
                let pkt = &buf[..n];
                match pkt[0] >> 4 {
                    4 => {
                        let dst = Ipv4Addr::new(pkt[16], pkt[17], pkt[18], pkt[19]);
                        let _ = router.deliver(dst, Frame::Packet(pkt.to_vec()));
                    }
                    6 if n >= 40 => {
                        // IPv6 destination address is bytes 24..40 of the header.
                        let mut o = [0u8; 16];
                        o.copy_from_slice(&pkt[24..40]);
                        let dst = Ipv6Addr::from(o);
                        let _ = router.deliver6(dst, Frame::Packet(pkt.to_vec()));
                    }
                    _ => {}
                }
            }
            Ok(_) => {}
            Err(e) => {
                tracing::error!("server TUN read failed: {e}");
                break;
            }
        }
    }
}

/// Dial the proxy target for an HTTP-proxy stream and pump bytes both ways.
/// Returns a sender the session uses to push client→remote data.
fn open_remote_stream(
    id: u32,
    target: TargetAddr,
    session_out: mpsc::Sender<Frame>,
) -> mpsc::Sender<Vec<u8>> {
    let (to_remote_tx, mut to_remote_rx) = mpsc::channel::<Vec<u8>>(256);
    tokio::spawn(async move {
        let addr = match &target {
            TargetAddr::Domain(h, p) => format!("{h}:{p}"),
            TargetAddr::Ip(sa) => sa.to_string(),
        };
        let remote = match tokio::net::TcpStream::connect(&addr).await {
            Ok(s) => s,
            Err(e) => {
                let _ = session_out
                    .send(Frame::StreamClose { id, error: Some(format!("dial {addr}: {e}")) })
                    .await;
                return;
            }
        };
        let _ = remote.set_nodelay(true);
        let (mut rd, mut wr) = remote.into_split();

        // remote → client
        let out = session_out.clone();
        let down = tokio::spawn(async move {
            let mut buf = vec![0u8; 16384];
            loop {
                match rd.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        if out
                            .send(Frame::StreamData { id, data: buf[..n].to_vec() })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            let _ = out.send(Frame::StreamClose { id, error: None }).await;
        });

        // client → remote
        while let Some(data) = to_remote_rx.recv().await {
            if wr.write_all(&data).await.is_err() {
                break;
            }
        }
        down.abort();
    });
    to_remote_tx
}

/// Handle one accepted client connection through to disconnect.
pub async fn handle_connection(state: AppState, accepted: Accepted) {
    let Accepted {
        peer_addr,
        sink,
        stream,
    } = accepted;
    let (mut writer, mut reader) = protocol::frames(sink, stream);

    let hello = match reader.recv().await {
        Ok(Frame::Hello(h)) => h,
        Ok(other) => {
            warn!(%peer_addr, "first frame was not Hello: {other:?}");
            return;
        }
        Err(e) => {
            debug!(%peer_addr, "handshake read failed: {e}");
            return;
        }
    };

    if hello.version != PROTOCOL_VERSION {
        let _ = writer
            .send(&Frame::Reject {
                reason: format!(
                    "protocol version mismatch (server {PROTOCOL_VERSION}, client {})",
                    hello.version
                ),
            })
            .await;
        return;
    }

    // Resolve the peer record by token.
    let peer = {
        let cfg = state.config.read().unwrap();
        cfg.find_peer(&hello.token).cloned()
    };
    let peer = match peer {
        Some(p) if p.enabled => p,
        Some(_) => {
            let _ = writer
                .send(&Frame::Reject {
                    reason: "peer is disabled".into(),
                })
                .await;
            return;
        }
        None => {
            warn!(%peer_addr, "rejected unknown token");
            let _ = writer
                .send(&Frame::Reject {
                    reason: "unknown token".into(),
                })
                .await;
            return;
        }
    };

    if matches!(hello.mode, SessionMode::GlobalProxy) && !peer.allow_global {
        let _ = writer
            .send(&Frame::Reject {
                reason: "global proxy not allowed for this peer".into(),
            })
            .await;
        return;
    }

    let (prefix_len, gateway, mtu, dns, subnet6, cfg_gateway6, prefix6, cfg_dns6) = {
        let cfg = state.config.read().unwrap();
        let (subnet6, prefix6) = cfg
            .subnet6_net()
            .map(|n| (Some(n), n.prefix_len()))
            .unwrap_or((None, 0));
        (
            cfg.prefix_len().unwrap_or(24),
            cfg.network.gateway,
            cfg.network.mtu,
            cfg.network.dns.clone(),
            subnet6,
            cfg.network.gateway6,
            prefix6,
            cfg.network.dns6.clone(),
        )
    };
    let assigned = peer.ip;
    // IPv6 (NAT66): derive the peer's v6 from its pinned v4 — but only advertise it
    // when the server actually brought up v6 egress (`state.ipv6`).
    let (assigned_ip6, gateway6, prefix6, dns6) = match (state.ipv6, &subnet6) {
        (true, Some(net6)) => (
            Some(derive_ipv6(net6, assigned)),
            cfg_gateway6,
            prefix6,
            cfg_dns6,
        ),
        _ => (None, None, 0, Vec::new()),
    };
    // VPN membership: `Vpn` mode is always a member; any other mode can opt in via
    // `join_vpn` (e.g. a proxy client that also wants to reach peers by IP).
    let vpn_member = matches!(hello.mode, SessionMode::Vpn) || hello.join_vpn;

    let (tx, mut rx) = mpsc::channel::<Frame>(1024);
    let counters = Arc::new(Counters::default());
    state.router.register(
        assigned,
        assigned_ip6,
        tx.clone(),
        Online {
            name: peer.name.clone(),
            mode: hello.mode,
            addr: peer_addr.to_string(),
            vpn_member,
            ip6: assigned_ip6,
        },
        peer.token.clone(),
        counters.clone(),
    );

    let welcome = Welcome {
        session_id: Uuid::new_v4(),
        assigned_ip: assigned,
        prefix_len,
        gateway,
        mtu,
        dns,
        assigned_ip6,
        prefix6,
        gateway6,
        dns6,
    };
    if writer.send(&Frame::Welcome(welcome)).await.is_err() {
        state.router.unregister(assigned, assigned_ip6);
        return;
    }
    info!(peer = %peer.name, ip = %assigned, mode = %hello.mode, from = %peer_addr, "session established");

    // Outbound writer task (frames queued by the router or this handler).
    // Counts server→client payload bytes for the metrics view.
    let counters_w = counters.clone();
    let metrics_w = state.metrics.clone();
    let writer_task = tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            let n = match &frame {
                Frame::Packet(p) => p.len() as u64,
                Frame::StreamData { data, .. } => data.len() as u64,
                _ => 0,
            };
            if writer.send(&frame).await.is_err() {
                break;
            }
            if n > 0 {
                counters_w.down.fetch_add(n, Ordering::Relaxed);
                metrics_w.add_down(n);
            }
        }
        let _ = writer.close().await;
    });

    let tun = state.tun.clone();
    // HTTP-proxy mode: id → sender feeding the remote-dialing task.
    let mut streams: HashMap<u32, mpsc::Sender<Vec<u8>>> = HashMap::new();
    loop {
        match reader.recv().await {
            Ok(Frame::Packet(pkt)) => {
                counters.up.fetch_add(pkt.len() as u64, Ordering::Relaxed);
                state.metrics.add_up(pkt.len() as u64);
                // Packet path: native packet modes, plus any session that joined
                // the VPN (a proxy-mode client carries peer LAN packets too).
                if !hello.mode.is_stream() || vpn_member {
                    if let Some(t) = &tun {
                        let _ = t.send(&pkt).await;
                    }
                }
            }
            Ok(Frame::Ping) => {
                let _ = tx.try_send(Frame::Pong);
            }
            Ok(Frame::Pong) => {}
            Ok(Frame::GetPeers) => {
                // The other VPN members on this server (in `Vpn` mode or joined via
                // `join_vpn`). Exclude this session itself so the count is "peers I
                // can reach". Proxy-mode sessions that did not join are not listed.
                let peers: Vec<PeerInfo> = state
                    .router
                    .online()
                    .into_iter()
                    .filter(|(ip, o)| *ip != assigned && o.vpn_member)
                    .map(|(ip, o)| PeerInfo { ip, name: o.name, ip6: o.ip6 })
                    .collect();
                let _ = tx.try_send(Frame::PeerList { peers });
            }
            Ok(Frame::StreamOpen { id, target }) => {
                if peer.allow_http_proxy {
                    streams.insert(id, open_remote_stream(id, target, tx.clone()));
                } else {
                    let _ = tx.try_send(Frame::StreamClose {
                        id,
                        error: Some("http proxy not allowed for this peer".into()),
                    });
                }
            }
            Ok(Frame::StreamData { id, data }) => {
                counters.up.fetch_add(data.len() as u64, Ordering::Relaxed);
                state.metrics.add_up(data.len() as u64);
                // try_send: a congested stream drops rather than blocking other
                // streams sharing this session's reader loop.
                if let Some(s) = streams.get(&id) {
                    let _ = s.try_send(data);
                }
            }
            Ok(Frame::StreamClose { id, .. }) => {
                streams.remove(&id);
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }

    state.router.unregister(assigned, assigned_ip6);
    writer_task.abort();
    // Preserve this session's traffic in the cumulative per-peer totals.
    state.metrics.fold_session(
        &peer.token,
        &peer.name,
        assigned,
        hello.mode,
        counters.up.load(Ordering::Relaxed),
        counters.down.load(Ordering::Relaxed),
    );
    info!(peer = %peer.name, ip = %assigned, "session closed");
}
