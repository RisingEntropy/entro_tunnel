//! The Android engine: establish the (optionally chained) tunnel with the shared
//! `entrotunnel-client` engine, then bridge packets between the server and the
//! `VpnService`-provided fd. No OS routing is done here — `VpnService.Builder`
//! (Kotlin side) applies the address / routes / DNS / split-tunnel.
//!
//! Because the `VpnService.Builder` needs the server-assigned virtual IP BEFORE
//! it can `establish()` the TUN, the flow is two-phase: `connect()` does the
//! handshake and returns the network config; the caller builds the VPN, then
//! hands the fd back via the oneshot so `fd_bridge` can run.

use entrotunnel_client::chain::{self, ChainTunnel};
use entrotunnel_client::config::ClientConfig;
use entrotunnel_client::engine::SharedStatus;
use entrotunnel_client::proxy;
use entrotunnel_core::config::SessionMode;
use entrotunnel_core::protocol::{Frame, FrameReader, FrameWriter, Welcome};
use entrotunnel_core::{Error, Result, KEEPALIVE_SECS};
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::unix::AsyncFd;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tracing::info;

/// How often a VPN-member client polls the server for the peer list.
const PEER_REFRESH_SECS: u64 = 5;

/// Resolve the hop list: an explicit chain (≥2 valid hops) or the single selected
/// server. `connect_chain` treats a 1-hop list as a direct connection.
fn hops_of(cfg: &ClientConfig) -> Result<Vec<entrotunnel_client::config::ServerEntry>> {
    if cfg.chain.len() >= 2 {
        let h = chain::resolve_hops(&cfg.chain, &cfg.servers);
        if h.len() >= 2 {
            return Ok(h);
        }
    }
    Ok(vec![cfg.active_server()?])
}

/// The network config the Kotlin `VpnService.Builder` needs, as JSON.
fn netcfg_json(welcome: &Welcome, mode: SessionMode) -> serde_json::Value {
    serde_json::json!({
        "mode": mode, // serializes to "global_proxy" / "vpn" / "http_proxy" / ...
        "assigned_ip": welcome.assigned_ip.to_string(),
        "prefix_len": welcome.prefix_len,
        "gateway": welcome.gateway.to_string(),
        "mtu": welcome.mtu,
        "dns": welcome.dns.iter().map(|d| d.to_string()).collect::<Vec<_>>(),
    })
}

/// Phase 1+2 of the session. Connects (single or chain), reports the network
/// config back through `cfg_tx`, then:
///  * packet modes — wait for the VpnService fd on `fd_rx`, then bridge it;
///  * stream (HTTP/system-proxy) modes — run the local proxy immediately.
pub(crate) async fn run(
    cfg: ClientConfig,
    cfg_tx: std::sync::mpsc::Sender<std::result::Result<serde_json::Value, String>>,
    fd_rx: oneshot::Receiver<RawFd>,
    cancel: CancellationToken,
    shared: SharedStatus,
) -> Result<()> {
    let hops = match hops_of(&cfg) {
        Ok(h) => h,
        Err(e) => {
            let _ = cfg_tx.send(Err(e.to_string()));
            return Err(e);
        }
    };
    info!(mode = %cfg.mode, hops = hops.len(), "android engine connecting");

    let tunnel = match chain::connect_chain(
        &hops,
        cfg.mode,
        cfg.requested_ip,
        cfg.client_name.clone(),
        &cancel,
    )
    .await
    {
        Ok(t) => t,
        Err(e) => {
            let _ = cfg_tx.send(Err(e.to_string()));
            return Err(e);
        }
    };
    let ChainTunnel {
        writer,
        reader,
        welcome,
        ..
    } = tunnel;
    if let Ok(mut s) = shared.lock() {
        s.assigned_ip = Some(welcome.assigned_ip);
    }
    let member = matches!(cfg.mode, SessionMode::Vpn) || cfg.join_vpn;
    let _ = cfg_tx.send(Ok(netcfg_json(&welcome, cfg.mode)));

    match cfg.mode {
        // Packet modes: wait for the VpnService fd, then bridge.
        SessionMode::GlobalProxy | SessionMode::Vpn => {
            let fd = match fd_rx.await {
                Ok(fd) => fd,
                Err(_) => return Ok(()), // cancelled before the VPN was built
            };
            fd_bridge(fd, welcome, writer, reader, member, shared, cancel).await
        }
        // Stream modes: apps point at the local HTTP proxy; no VpnService.
        SessionMode::HttpProxy | SessionMode::SystemProxy => {
            proxy::run_http_mode(&cfg, welcome, writer, reader, false, shared, cancel).await
        }
    }
}

/// A `RawFd` we do NOT own: it belongs to the Kotlin `ParcelFileDescriptor` /
/// `VpnService`. We must not close it on drop (that side does), so there is no
/// `Drop` impl here.
struct TunFd(RawFd);
impl AsRawFd for TunFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

fn set_nonblocking(fd: RawFd) -> std::io::Result<()> {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

async fn tun_read(afd: &AsyncFd<TunFd>, buf: &mut [u8]) -> Result<usize> {
    loop {
        let mut guard = afd.readable().await.map_err(Error::Io)?;
        match guard.try_io(|inner| {
            let n = unsafe {
                libc::read(
                    inner.get_ref().as_raw_fd(),
                    buf.as_mut_ptr() as *mut libc::c_void,
                    buf.len(),
                )
            };
            if n < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(n as usize)
            }
        }) {
            Ok(res) => return res.map_err(Error::Io),
            Err(_would_block) => continue,
        }
    }
}

async fn tun_write(afd: &AsyncFd<TunFd>, pkt: &[u8]) -> Result<usize> {
    loop {
        let mut guard = afd.writable().await.map_err(Error::Io)?;
        match guard.try_io(|inner| {
            let n = unsafe {
                libc::write(
                    inner.get_ref().as_raw_fd(),
                    pkt.as_ptr() as *const libc::c_void,
                    pkt.len(),
                )
            };
            if n < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(n as usize)
            }
        }) {
            Ok(res) => return res.map_err(Error::Io),
            Err(_would_block) => continue,
        }
    }
}

/// Bridge IP packets between the VpnService fd and the (chained) server tunnel.
async fn fd_bridge(
    fd: RawFd,
    welcome: Welcome,
    mut writer: FrameWriter,
    mut reader: FrameReader,
    member: bool,
    shared: SharedStatus,
    cancel: CancellationToken,
) -> Result<()> {
    if fd < 0 {
        return Err(Error::Config("no VpnService fd provided for packet mode".into()));
    }
    set_nonblocking(fd).map_err(Error::Io)?;
    let tun = Arc::new(AsyncFd::new(TunFd(fd)).map_err(Error::Io)?);
    let read_len = (welcome.mtu as usize).max(2048);

    // Uplink: TUN → server, with keepalive and (VPN) peer polling.
    let tun_up = tun.clone();
    let cancel_up = cancel.clone();
    let uplink = tokio::spawn(async move {
        let mut buf = vec![0u8; read_len];
        let mut keepalive = tokio::time::interval(Duration::from_secs(KEEPALIVE_SECS));
        keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut peers = tokio::time::interval(Duration::from_secs(PEER_REFRESH_SECS));
        peers.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = cancel_up.cancelled() => break,
                _ = keepalive.tick() => {
                    if writer.send(&Frame::Ping).await.is_err() { break; }
                }
                _ = peers.tick(), if member => {
                    if writer.send(&Frame::GetPeers).await.is_err() { break; }
                }
                r = tun_read(&tun_up, &mut buf) => match r {
                    Ok(n) if n > 0 => {
                        if writer.send(&Frame::Packet(buf[..n].to_vec())).await.is_err() { break; }
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        }
        let _ = writer.close().await;
    });

    // Downlink: server → TUN, caching any VPN peer list.
    let tun_down = tun.clone();
    let cancel_down = cancel.clone();
    let shared_down = shared.clone();
    let downlink = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel_down.cancelled() => break,
                f = reader.recv() => match f {
                    Ok(Frame::Packet(pkt)) => { let _ = tun_write(&tun_down, &pkt).await; }
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
    info!("android session closed");
    Ok(())
}
