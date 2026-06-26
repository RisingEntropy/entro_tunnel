//! Transport layer: an encrypted, ordered, reliable channel of byte *messages*.
//!
//! Every transport, after its encryption handshake, yields a
//! ([`MessageSink`], [`MessageStream`]) pair. The two halves are separate so the
//! client/server pumps can read and write concurrently from different tasks.

use crate::config::TransportKind;
use crate::{Error, Result};
use async_trait::async_trait;
use std::net::SocketAddr;
use std::sync::Arc;

pub mod framing;

#[cfg(feature = "tcp")]
pub mod tcp;
#[cfg(feature = "ws")]
pub mod ws;
#[cfg(feature = "quic")]
pub mod quic;

/// Write half of an encrypted message channel.
#[async_trait]
pub trait MessageSink: Send {
    /// Send one message. Implementations encrypt + frame it.
    async fn send(&mut self, msg: &[u8]) -> Result<()>;
    /// Close the write side. Default no-op.
    async fn close(&mut self) -> Result<()> {
        Ok(())
    }
}

/// Read half of an encrypted message channel.
#[async_trait]
pub trait MessageStream: Send {
    /// Receive one message. Returns [`Error::Closed`] on clean EOF.
    async fn recv(&mut self) -> Result<Vec<u8>>;
}

pub type BoxSink = Box<dyn MessageSink>;
pub type BoxStream = Box<dyn MessageStream>;

/// Where a client dials.
#[derive(Clone, Debug)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
    pub kind: TransportKind,
    /// WebSocket request path (ignored for tcp/quic).
    pub ws_path: String,
    /// TLS SNI / server name (ws/quic).
    pub server_name: String,
}

/// Client-side security material.
#[derive(Clone)]
pub struct ClientSecurity {
    /// Pre-shared key for the Noise (raw-TCP) channel.
    pub noise_psk: [u8; 32],
    /// For TLS transports: skip certificate verification (self-hosted, logged).
    pub tls_skip_verify: bool,
    /// For TLS transports: pin this exact certificate (PEM) instead.
    pub tls_pinned_cert_pem: Option<String>,
}

/// Server-side security material.
pub struct ServerSecurity {
    pub noise_psk: [u8; 32],
    pub tls_cert_pem: String,
    pub tls_key_pem: String,
}

/// A freshly accepted server-side connection (handshake already done).
pub struct Accepted {
    pub peer_addr: SocketAddr,
    pub sink: BoxSink,
    pub stream: BoxStream,
}

/// A pending server-side connection whose encryption handshake has NOT run yet.
/// [`Listener::accept`] returns this after only the cheap kernel-level accept;
/// run [`Incoming::finish`] — *off* the accept loop and under a timeout — to
/// perform the (blocking) handshake, so a peer that stalls mid-handshake can
/// never wedge the listener.
pub enum Incoming {
    #[cfg(feature = "tcp")]
    Tcp(tcp::TcpIncoming),
    #[cfg(feature = "ws")]
    Ws(ws::WsIncoming),
    #[cfg(feature = "quic")]
    Quic(quic::QuicIncoming),
}

impl Incoming {
    /// Complete the transport's encryption handshake. Run this off the accept
    /// loop, wrapped in a timeout.
    pub async fn finish(self) -> Result<Accepted> {
        match self {
            #[cfg(feature = "tcp")]
            Incoming::Tcp(i) => i.finish().await,
            #[cfg(feature = "ws")]
            Incoming::Ws(i) => i.finish().await,
            #[cfg(feature = "quic")]
            Incoming::Quic(i) => i.finish().await,
            #[allow(unreachable_patterns)]
            _ => Err(Error::NotImplemented("no transport features enabled")),
        }
    }
}

/// Dial a server, performing the transport's encryption handshake.
pub async fn connect(ep: &Endpoint, sec: &ClientSecurity) -> Result<(BoxSink, BoxStream)> {
    match ep.kind {
        TransportKind::Tcp => {
            #[cfg(feature = "tcp")]
            {
                tcp::connect(ep, sec).await
            }
            #[cfg(not(feature = "tcp"))]
            {
                let _ = sec;
                Err(Error::NotImplemented("tcp feature disabled in this build"))
            }
        }
        TransportKind::Ws => {
            #[cfg(feature = "ws")]
            {
                ws::connect(ep, sec).await
            }
            #[cfg(not(feature = "ws"))]
            {
                let _ = sec;
                Err(Error::NotImplemented("ws feature disabled in this build"))
            }
        }
        TransportKind::Quic => {
            #[cfg(feature = "quic")]
            {
                quic::connect(ep, sec).await
            }
            #[cfg(not(feature = "quic"))]
            {
                let _ = sec;
                Err(Error::NotImplemented("quic feature disabled in this build"))
            }
        }
    }
}

/// Run a transport's encryption handshake over an already-established byte stream
/// (a relayed proxy-chain carrier) instead of dialing a fresh socket. Used for
/// every hop after the first. QUIC is UDP-based and cannot tunnel over a stream,
/// so it is only valid as the first (directly-dialed) hop.
pub async fn connect_over<S>(
    stream: S,
    ep: &Endpoint,
    sec: &ClientSecurity,
) -> Result<(BoxSink, BoxStream)>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    match ep.kind {
        TransportKind::Tcp => {
            #[cfg(feature = "tcp")]
            {
                tcp::connect_over(stream, sec).await
            }
            #[cfg(not(feature = "tcp"))]
            {
                let _ = (stream, sec);
                Err(Error::NotImplemented("tcp feature disabled in this build"))
            }
        }
        TransportKind::Ws => {
            #[cfg(feature = "ws")]
            {
                ws::connect_over(stream, ep, sec).await
            }
            #[cfg(not(feature = "ws"))]
            {
                let _ = (stream, ep, sec);
                Err(Error::NotImplemented("ws feature disabled in this build"))
            }
        }
        TransportKind::Quic => {
            let _ = (stream, ep, sec);
            Err(Error::NotImplemented(
                "QUIC can only be the first hop of a proxy chain (UDP can't tunnel over a relayed stream)",
            ))
        }
    }
}

/// A bound server listener for a single transport/port.
pub enum Listener {
    #[cfg(feature = "tcp")]
    Tcp(tcp::TcpNoiseListener),
    #[cfg(feature = "ws")]
    Ws(ws::WsListener),
    #[cfg(feature = "quic")]
    Quic(quic::QuicListener),
}

impl Listener {
    /// `ws_tls` toggles whether a WebSocket listener terminates TLS itself
    /// (`true` → WSS) or accepts plain WS (`false` → TLS terminated by a front
    /// proxy like nginx). Ignored for tcp (always Noise) and quic (always TLS).
    pub async fn bind(
        addr: SocketAddr,
        kind: TransportKind,
        sec: Arc<ServerSecurity>,
        ws_tls: bool,
    ) -> Result<Self> {
        match kind {
            TransportKind::Tcp => {
                #[cfg(feature = "tcp")]
                {
                    Ok(Listener::Tcp(tcp::TcpNoiseListener::bind(addr, sec).await?))
                }
                #[cfg(not(feature = "tcp"))]
                {
                    let _ = (addr, sec);
                    Err(Error::NotImplemented("tcp feature disabled"))
                }
            }
            TransportKind::Ws => {
                #[cfg(feature = "ws")]
                {
                    Ok(Listener::Ws(ws::WsListener::bind(addr, sec, ws_tls).await?))
                }
                #[cfg(not(feature = "ws"))]
                {
                    let _ = (addr, sec, ws_tls);
                    Err(Error::NotImplemented("ws feature disabled"))
                }
            }
            TransportKind::Quic => {
                #[cfg(feature = "quic")]
                {
                    Ok(Listener::Quic(quic::QuicListener::bind(addr, sec).await?))
                }
                #[cfg(not(feature = "quic"))]
                {
                    let _ = (addr, sec);
                    Err(Error::NotImplemented("quic feature disabled"))
                }
            }
        }
    }

    /// Pull the next pending connection. This does only the fast kernel-level
    /// accept — NOT the handshake — so the accept loop never blocks on a slow or
    /// stalled peer. Run [`Incoming::finish`] off the loop, under a timeout.
    pub async fn accept(&self) -> Result<Incoming> {
        match self {
            #[cfg(feature = "tcp")]
            Listener::Tcp(l) => Ok(Incoming::Tcp(l.accept().await?)),
            #[cfg(feature = "ws")]
            Listener::Ws(l) => Ok(Incoming::Ws(l.accept().await?)),
            #[cfg(feature = "quic")]
            Listener::Quic(l) => Ok(Incoming::Quic(l.accept().await?)),
            #[allow(unreachable_patterns)]
            _ => Err(Error::NotImplemented("no transport features enabled")),
        }
    }
}
