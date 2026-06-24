//! Proxy-chain orchestration: client → S1 → S2 → … → Sn → internet.
//!
//! Each hop runs the *unmodified* EntroTunnel server. The first hop is dialed
//! directly; every later hop is reached by asking the previous hop (in its
//! stream-relay / HTTP-proxy mode) to open a raw connection to the next hop's
//! address, then layering that hop's own transport handshake on top of the
//! relayed byte stream. Only the final hop runs the chosen egress mode.
//!
//! The carrier is built with `tokio::io::duplex` so the next hop's transport sees
//! a normal `AsyncRead + AsyncWrite`; a small bridge moves bytes between it and
//! `StreamData` frames on the previous hop's tunnel. No server changes are needed.

use crate::config::ServerEntry;
use entrotunnel_core::config::{parse_psk, SessionMode};
use entrotunnel_core::protocol::{self, Frame, FrameReader, FrameWriter, Hello, TargetAddr, Welcome};
use entrotunnel_core::transport::{self, ClientSecurity, Endpoint};
use entrotunnel_core::{Error, Result, PROTOCOL_VERSION};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// The single multiplexed stream id used on each carrier tunnel (one hop carries
/// exactly one next-hop connection).
const CARRIER_ID: u32 = 1;

/// A fully-established chain: the frame channel to the LAST hop, plus its Welcome.
pub struct ChainTunnel {
    pub writer: FrameWriter,
    pub reader: FrameReader,
    pub welcome: Welcome,
    /// The directly-dialed first hop's resolved IP (for the server-pin route).
    pub first_hop_ip: IpAddr,
}

fn endpoint_of(s: &ServerEntry) -> Endpoint {
    Endpoint {
        host: s.host.clone(),
        port: s.port,
        kind: s.transport,
        ws_path: "/et".to_string(),
        server_name: s.sni(),
    }
}

fn security_of(s: &ServerEntry) -> Result<ClientSecurity> {
    Ok(ClientSecurity {
        noise_psk: parse_psk(&s.noise_psk)?,
        tls_skip_verify: s.tls_skip_verify,
        tls_pinned_cert_pem: None,
    })
}

fn target_of(s: &ServerEntry) -> TargetAddr {
    if let Ok(ip) = s.host.parse::<IpAddr>() {
        TargetAddr::Ip(SocketAddr::new(ip, s.port))
    } else {
        TargetAddr::Domain(s.host.clone(), s.port)
    }
}

async fn resolve(host: &str, port: u16) -> Result<IpAddr> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(ip);
    }
    tokio::net::lookup_host((host, port))
        .await?
        .next()
        .map(|s| s.ip())
        .ok_or_else(|| Error::Config(format!("cannot resolve host {host}")))
}

/// Send Hello and await Welcome for one hop.
async fn handshake(
    writer: &mut FrameWriter,
    reader: &mut FrameReader,
    srv: &ServerEntry,
    mode: SessionMode,
    requested_ip: Option<Ipv4Addr>,
    client_name: Option<String>,
) -> Result<Welcome> {
    let hello = Hello {
        version: PROTOCOL_VERSION,
        token: srv.token.clone(),
        mode: mode.wire(),
        requested_ip,
        client_name,
        // A chain hop never joins the peer LAN over its own (relay/egress) tunnel;
        // the VPN LAN is a separate direct connection to the first hop.
        join_vpn: false,
    };
    writer.send(&Frame::Hello(hello)).await?;
    match reader.recv().await? {
        Frame::Welcome(w) => Ok(w),
        Frame::Reject { reason } => Err(Error::Auth(reason)),
        other => Err(Error::Protocol(format!("expected Welcome, got {other:?}"))),
    }
}

/// Open one relayed stream over a hop's tunnel and return a duplex endpoint the
/// next hop's transport can run over. The previous hop's `writer`/`reader` are
/// consumed by background pumps that live until `cancel` fires (or the link dies).
async fn open_carrier(
    mut writer: FrameWriter,
    mut reader: FrameReader,
    target: TargetAddr,
    cancel: CancellationToken,
) -> Result<tokio::io::DuplexStream> {
    let (out_tx, mut out_rx) = mpsc::channel::<Frame>(1024);
    let (near, far) = tokio::io::duplex(64 * 1024);
    let (mut far_r, mut far_w) = tokio::io::split(far);

    // Ask the previous hop to dial the next hop.
    out_tx
        .send(Frame::StreamOpen { id: CARRIER_ID, target })
        .await
        .map_err(|_| Error::Transport("carrier closed before open".into()))?;

    // Writer pump: queued frames → previous hop's tunnel.
    let c1 = cancel.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = c1.cancelled() => break,
                f = out_rx.recv() => match f {
                    Some(fr) => if writer.send(&fr).await.is_err() { break; },
                    None => break,
                }
            }
        }
        let _ = writer.close().await;
    });

    // Uplink: bytes the next hop writes (to `near`) → StreamData on the carrier.
    let out_up = out_tx.clone();
    let c2 = cancel.clone();
    tokio::spawn(async move {
        let mut buf = vec![0u8; 32 * 1024];
        loop {
            tokio::select! {
                _ = c2.cancelled() => break,
                r = far_r.read(&mut buf) => match r {
                    Ok(0) => break,
                    Ok(n) => {
                        if out_up
                            .send(Frame::StreamData { id: CARRIER_ID, data: buf[..n].to_vec() })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        }
        let _ = out_up
            .send(Frame::StreamClose { id: CARRIER_ID, error: None })
            .await;
    });

    // Downlink: StreamData from the carrier → bytes the next hop reads (from `near`).
    let out_pong = out_tx.clone();
    let c3 = cancel.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = c3.cancelled() => break,
                f = reader.recv() => match f {
                    Ok(Frame::StreamData { id, data }) if id == CARRIER_ID => {
                        if far_w.write_all(&data).await.is_err() { break; }
                    }
                    Ok(Frame::StreamClose { id, .. }) if id == CARRIER_ID => break,
                    Ok(Frame::Ping) => { let _ = out_pong.send(Frame::Pong).await; }
                    Ok(_) => {} // other ids / keepalive — ignore
                    Err(_) => break,
                }
            }
        }
        // far_w drops → `near` sees EOF.
    });

    Ok(near)
}

/// Build the whole chain and return the channel to the final hop. `hops` is the
/// ordered list of servers (first = directly dialed, last = egress). `final_mode`
/// is the mode the LAST hop runs; intermediate hops run stream-relay (HTTP-proxy).
pub async fn connect_chain(
    hops: &[ServerEntry],
    final_mode: SessionMode,
    requested_ip: Option<Ipv4Addr>,
    client_name: Option<String>,
    cancel: &CancellationToken,
) -> Result<ChainTunnel> {
    if hops.is_empty() {
        return Err(Error::Config("proxy chain is empty".into()));
    }
    let last = hops.len() - 1;
    let first_hop_ip = resolve(&hops[0].host, hops[0].port).await?;

    info!(
        chain = %hops.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(" → "),
        egress = %final_mode,
        "building proxy chain"
    );

    // First hop: dial directly.
    let ep0 = endpoint_of(&hops[0]);
    let sec0 = security_of(&hops[0])?;
    let (sink, stream) = transport::connect(&ep0, &sec0).await?;
    let (mut writer, mut reader) = protocol::frames(sink, stream);
    let mode0 = if last == 0 { final_mode } else { SessionMode::HttpProxy };
    let rip0 = if last == 0 { requested_ip } else { None };
    let mut welcome = handshake(&mut writer, &mut reader, &hops[0], mode0, rip0, client_name.clone()).await?;

    // Remaining hops: relayed through the previous hop.
    for k in 1..hops.len() {
        let target = target_of(&hops[k]);
        let near = open_carrier(writer, reader, target, cancel.clone()).await?;
        let epk = endpoint_of(&hops[k]);
        let seck = security_of(&hops[k])?;
        let (sink, stream) = transport::connect_over(near, &epk, &seck).await?;
        let (w, r) = protocol::frames(sink, stream);
        writer = w;
        reader = r;
        let mode_k = if k == last { final_mode } else { SessionMode::HttpProxy };
        let rip = if k == last { requested_ip } else { None };
        welcome = handshake(&mut writer, &mut reader, &hops[k], mode_k, rip, client_name.clone()).await?;
        info!(hop = %hops[k].name, "chain hop established");
    }

    Ok(ChainTunnel { writer, reader, welcome, first_hop_ip })
}

/// Resolve a list of server names (a chain order) against a profile's servers.
/// Unknown names are dropped with a warning; the result preserves order.
pub fn resolve_hops(chain: &[String], servers: &[ServerEntry]) -> Vec<ServerEntry> {
    let mut out = Vec::new();
    for name in chain {
        match servers.iter().find(|s| &s.name == name) {
            Some(s) => out.push(s.clone()),
            None => warn!("chain references unknown server '{name}'; skipped"),
        }
    }
    out
}
