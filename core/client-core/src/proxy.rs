//! Local HTTP proxy listener for **HTTP-proxy mode** (no TUN; runs unprivileged).
//!
//! Accepts HTTP `CONNECT` (HTTPS tunnelling) and absolute-form HTTP requests,
//! multiplexes each local TCP connection onto the single encrypted server link
//! as a `Frame::StreamOpen` / `StreamData` / `StreamClose` stream. The server
//! dials the target and pumps bytes back.

use crate::config::ClientConfig;
use crate::engine::{add_traffic_down, add_traffic_up, SharedStatus, PEER_REFRESH_SECS};
use crate::tun::{TunConfig, TunDevice};
use entrotunnel_core::protocol::{Frame, FrameReader, FrameWriter, TargetAddr, Welcome};
use entrotunnel_core::{Error, Result, KEEPALIVE_SECS};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

/// Message delivered from the server demux task to a local connection handler.
enum ToLocal {
    Data(Vec<u8>),
    Close,
}

type StreamMap = Arc<Mutex<HashMap<u32, mpsc::Sender<ToLocal>>>>;

pub async fn run_http_mode(
    cfg: &ClientConfig,
    welcome: Welcome,
    writer: FrameWriter,
    reader: FrameReader,
    // VPN member (in a proxy mode this is only true via `join_vpn`): also bring up
    // the peer LAN. Same name/meaning as `member` in the packet-mode path.
    member: bool,
    shared: SharedStatus,
    cancel: CancellationToken,
) -> Result<()> {
    let listener = TcpListener::bind(&cfg.http_listen)
        .await
        .map_err(|e| Error::Config(format!("bind {}: {e}", cfg.http_listen)))?;
    info!(listen = %cfg.http_listen, "HTTP proxy listening");

    let (out_tx, out_rx) = mpsc::channel::<Frame>(1024);
    let streams: StreamMap = Arc::new(Mutex::new(HashMap::new()));

    // Optionally also join the VPN peer LAN. We bring up a TUN routed at *only*
    // the virtual subnet (no default route / DNS change, so internet still goes
    // through this proxy mode), bridge it onto the same encrypted link, and poll
    // the server for the peer list. `_lan_guard` restores routes when dropped.
    let mut tun: Option<Arc<TunDevice>> = None;
    let mut _lan_guard: Option<crate::netcfg::NetGuard> = None;
    let mut aux_tasks: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    if member {
        let dev = Arc::new(
            TunDevice::create(&TunConfig {
                name: cfg.tun_name.clone(),
                ip: welcome.assigned_ip,
                prefix_len: welcome.prefix_len,
                mtu: welcome.mtu,
            })
            .await?,
        );
        info!(dev = %dev.name(), "VPN LAN TUN up (proxy mode + join VPN)");
        _lan_guard = Some(crate::netcfg::apply_vpn_lan(&welcome, dev.name()).await?);

        // Uplink: LAN packets (TUN) → server, multiplexed onto the same link.
        let tun_up = dev.clone();
        let out_up = out_tx.clone();
        let cancel_up = cancel.clone();
        let buf_len = (welcome.mtu as usize).max(2048);
        aux_tasks.push(tokio::spawn(async move {
            let mut buf = vec![0u8; buf_len];
            loop {
                tokio::select! {
                    _ = cancel_up.cancelled() => break,
                    r = tun_up.recv(&mut buf) => match r {
                        Ok(n) if n > 0 => {
                            if out_up.send(Frame::Packet(buf[..n].to_vec())).await.is_err() { break; }
                        }
                        Ok(_) => {}
                        Err(e) => { debug!("vpn tun recv: {e}"); break; }
                    }
                }
            }
        }));

        // Periodically ask the server who else is on the VPN.
        let out_pp = out_tx.clone();
        let cancel_pp = cancel.clone();
        aux_tasks.push(tokio::spawn(async move {
            let mut iv = tokio::time::interval(Duration::from_secs(PEER_REFRESH_SECS));
            iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = cancel_pp.cancelled() => break,
                    _ = iv.tick() => {
                        if out_pp.send(Frame::GetPeers).await.is_err() { break; }
                    }
                }
            }
        }));

        tun = Some(dev);
    }

    let writer_task = tokio::spawn(writer_loop(writer, out_rx, shared.clone(), cancel.clone()));
    let reader_task = tokio::spawn(reader_loop(
        reader,
        streams.clone(),
        out_tx.clone(),
        tun,
        shared,
        cancel.clone(),
    ));

    let next_id = Arc::new(AtomicU32::new(1));
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            accepted = listener.accept() => {
                let (sock, _peer) = match accepted { Ok(v) => v, Err(_) => continue };
                let id = next_id.fetch_add(1, Ordering::Relaxed);
                let out = out_tx.clone();
                let streams = streams.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_local(sock, id, out, streams).await {
                        debug!("http conn {id}: {e}");
                    }
                });
            }
        }
    }
    writer_task.abort();
    reader_task.abort();
    for t in aux_tasks {
        t.abort();
    }
    Ok(())
}

/// Single owner of the FrameWriter: drains queued frames + sends keepalives.
async fn writer_loop(
    mut writer: FrameWriter,
    mut out_rx: mpsc::Receiver<Frame>,
    shared: SharedStatus,
    cancel: CancellationToken,
) {
    let mut keepalive = tokio::time::interval(Duration::from_secs(KEEPALIVE_SECS));
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = keepalive.tick() => { if writer.send(&Frame::Ping).await.is_err() { break; } }
            frame = out_rx.recv() => match frame {
                Some(f) => {
                    let n = match &f {
                        Frame::Packet(p) => p.len(),
                        Frame::StreamData { data, .. } => data.len(),
                        _ => 0,
                    };
                    if writer.send(&f).await.is_err() { break; }
                    add_traffic_up(&shared, n);
                }
                None => break,
            }
        }
    }
    let _ = writer.close().await;
}

/// Demultiplexes server frames: proxy streams back to their local connections,
/// plus (when joined to the VPN) raw packets to the LAN TUN and peer-list updates
/// into the shared status.
async fn reader_loop(
    mut reader: FrameReader,
    streams: StreamMap,
    out_tx: mpsc::Sender<Frame>,
    tun: Option<Arc<TunDevice>>,
    shared: SharedStatus,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            frame = reader.recv() => match frame {
                Ok(Frame::StreamData { id, data }) => {
                    let n = data.len();
                    let tx = streams.lock().unwrap().get(&id).cloned();
                    if let Some(tx) = tx {
                        let _ = tx.send(ToLocal::Data(data)).await;
                    }
                    add_traffic_down(&shared, n);
                }
                Ok(Frame::StreamClose { id, .. }) => {
                    let tx = streams.lock().unwrap().remove(&id);
                    if let Some(tx) = tx {
                        let _ = tx.send(ToLocal::Close).await;
                    }
                }
                Ok(Frame::Ping) => { let _ = out_tx.send(Frame::Pong).await; }
                // VPN peer LAN (only present when joined): packets to the TUN.
                Ok(Frame::Packet(pkt)) => {
                    let n = pkt.len();
                    if let Some(t) = &tun { let _ = t.send(&pkt).await; }
                    add_traffic_down(&shared, n);
                }
                Ok(Frame::PeerList { peers }) => {
                    if let Ok(mut s) = shared.lock() { s.peers = peers; }
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
    }
}

async fn handle_local(
    mut sock: TcpStream,
    id: u32,
    out: mpsc::Sender<Frame>,
    streams: StreamMap,
) -> Result<()> {
    let (target, is_connect, forward) = parse_http_head(&mut sock).await?;

    let (to_local_tx, mut to_local_rx) = mpsc::channel::<ToLocal>(256);
    streams.lock().unwrap().insert(id, to_local_tx);

    out.send(Frame::StreamOpen { id, target })
        .await
        .map_err(|_| Error::Closed)?;

    if is_connect {
        sock.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await?;
    } else if !forward.is_empty() {
        // Plain HTTP: forward the request head/body we already consumed.
        out.send(Frame::StreamData { id, data: forward })
            .await
            .map_err(|_| Error::Closed)?;
    }

    let (mut rd, mut wr) = sock.into_split();

    // Uplink: local socket → server.
    let out_up = out.clone();
    let uplink = tokio::spawn(async move {
        let mut buf = vec![0u8; 16384];
        loop {
            match rd.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    if out_up
                        .send(Frame::StreamData {
                            id,
                            data: buf[..n].to_vec(),
                        })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = out_up.send(Frame::StreamClose { id, error: None }).await;
    });

    // Downlink: server → local socket.
    while let Some(msg) = to_local_rx.recv().await {
        match msg {
            ToLocal::Data(d) => {
                if wr.write_all(&d).await.is_err() {
                    break;
                }
            }
            ToLocal::Close => break,
        }
    }

    uplink.abort();
    streams.lock().unwrap().remove(&id);
    Ok(())
}

fn find_sub(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Read the HTTP request head; return (target, is_connect, bytes_to_forward).
async fn parse_http_head(sock: &mut TcpStream) -> Result<(TargetAddr, bool, Vec<u8>)> {
    let mut buf = Vec::with_capacity(1024);
    let mut tmp = [0u8; 2048];
    loop {
        let n = sock.read(&mut tmp).await?;
        if n == 0 {
            return Err(Error::Protocol("eof before request head".into()));
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find_sub(&buf, b"\r\n\r\n") {
            let head = &buf[..pos + 4];
            let (target, is_connect) = parse_request_line(head)?;
            // CONNECT: discard the head (tunnel starts after our 200 reply).
            // Plain HTTP: forward everything (absolute-form request) to the origin.
            let forward = if is_connect { Vec::new() } else { buf.clone() };
            return Ok((target, is_connect, forward));
        }
        if buf.len() > 64 * 1024 {
            return Err(Error::Protocol("request head too large".into()));
        }
    }
}

fn parse_request_line(head: &[u8]) -> Result<(TargetAddr, bool)> {
    let text = String::from_utf8_lossy(head);
    let first = text.lines().next().unwrap_or("");
    let mut it = first.split_whitespace();
    let method = it.next().unwrap_or("");
    let uri = it.next().unwrap_or("");

    if method.eq_ignore_ascii_case("CONNECT") {
        let (h, p) = split_host_port(uri, 443);
        return Ok((TargetAddr::Domain(h, p), true));
    }

    let host_port = if let Some(rest) = uri.strip_prefix("http://") {
        rest.split('/').next().unwrap_or("").to_string()
    } else {
        text.lines()
            .find_map(|l| {
                let l = l.trim();
                l.strip_prefix("Host:")
                    .or_else(|| l.strip_prefix("host:"))
                    .map(|h| h.trim().to_string())
            })
            .unwrap_or_default()
    };
    if host_port.is_empty() {
        return Err(Error::Protocol(format!(
            "cannot parse proxy target: {first:?}"
        )));
    }
    let (h, p) = split_host_port(&host_port, 80);
    Ok((TargetAddr::Domain(h, p), false))
}

fn split_host_port(s: &str, default: u16) -> (String, u16) {
    if let Some((h, p)) = s.rsplit_once(':') {
        if let Ok(port) = p.parse::<u16>() {
            return (h.to_string(), port);
        }
    }
    (s.to_string(), default)
}
