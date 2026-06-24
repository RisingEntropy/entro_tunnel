//! WebSocket-over-TLS (WSS) transport.
//!
//! TLS 1.3 (rustls) provides encryption; each binary WS frame is one channel
//! message (no extra length prefix). The `noise_psk` is unused on this path.

use super::{Accepted, ClientSecurity, Endpoint, MessageSink, MessageStream, ServerSecurity};
use crate::crypto::tls;
use crate::{Error, Result};
use async_trait::async_trait;
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::{TlsAcceptor, TlsConnector};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

fn ws_err(e: tokio_tungstenite::tungstenite::Error) -> Error {
    use tokio_tungstenite::tungstenite::Error as E;
    match e {
        E::ConnectionClosed | E::AlreadyClosed => Error::Closed,
        other => Error::Transport(format!("ws: {other}")),
    }
}

struct WsSink<S> {
    inner: SplitSink<WebSocketStream<S>, Message>,
}

#[async_trait]
impl<S: AsyncRead + AsyncWrite + Unpin + Send> MessageSink for WsSink<S> {
    async fn send(&mut self, msg: &[u8]) -> Result<()> {
        self.inner
            .send(Message::Binary(msg.to_vec().into()))
            .await
            .map_err(ws_err)
    }
    async fn close(&mut self) -> Result<()> {
        let _ = self.inner.send(Message::Close(None)).await;
        let _ = self.inner.close().await;
        Ok(())
    }
}

struct WsRx<S> {
    inner: SplitStream<WebSocketStream<S>>,
}

#[async_trait]
impl<S: AsyncRead + AsyncWrite + Unpin + Send> MessageStream for WsRx<S> {
    async fn recv(&mut self) -> Result<Vec<u8>> {
        loop {
            match self.inner.next().await {
                Some(Ok(Message::Binary(b))) => return Ok(b.to_vec()),
                Some(Ok(Message::Close(_))) | None => return Err(Error::Closed),
                Some(Ok(_)) => continue, // text / ping / pong (auto-ponged)
                Some(Err(e)) => return Err(ws_err(e)),
            }
        }
    }
}

fn split<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
    ws: WebSocketStream<S>,
) -> (Box<dyn MessageSink>, Box<dyn MessageStream>) {
    let (tx, rx) = ws.split();
    (Box::new(WsSink { inner: tx }), Box::new(WsRx { inner: rx }))
}

pub async fn connect(
    ep: &Endpoint,
    sec: &ClientSecurity,
) -> Result<(Box<dyn MessageSink>, Box<dyn MessageStream>)> {
    let cfg = tls::client_config(sec.tls_skip_verify, sec.tls_pinned_cert_pem.as_deref())?;
    let connector = TlsConnector::from(cfg);

    let tcp = TcpStream::connect((ep.host.as_str(), ep.port)).await?;
    tcp.set_nodelay(true).ok();
    let server_name = rustls::pki_types::ServerName::try_from(ep.server_name.clone())
        .map_err(|e| Error::Crypto(format!("invalid server name: {e}")))?;
    let tls = connector
        .connect(server_name, tcp)
        .await
        .map_err(|e| Error::Crypto(format!("tls connect: {e}")))?;

    let url = format!("wss://{}:{}{}", ep.server_name, ep.port, ep.ws_path);
    let (ws, _resp) = tokio_tungstenite::client_async(url, tls)
        .await
        .map_err(ws_err)?;
    Ok(split(ws))
}

/// Run the WSS (TLS + WebSocket) client handshake over an already-established
/// byte stream (e.g. a relayed chain carrier) instead of a fresh TCP socket.
/// The relaying hop has already TCP-connected to `ep.host:ep.port`; here we layer
/// TLS (to the real cert / front proxy) and the WS upgrade on top.
pub async fn connect_over<S>(
    stream: S,
    ep: &Endpoint,
    sec: &ClientSecurity,
) -> Result<(Box<dyn MessageSink>, Box<dyn MessageStream>)>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let cfg = tls::client_config(sec.tls_skip_verify, sec.tls_pinned_cert_pem.as_deref())?;
    let connector = TlsConnector::from(cfg);
    let server_name = rustls::pki_types::ServerName::try_from(ep.server_name.clone())
        .map_err(|e| Error::Crypto(format!("invalid server name: {e}")))?;
    let tls = connector
        .connect(server_name, stream)
        .await
        .map_err(|e| Error::Crypto(format!("tls connect: {e}")))?;

    let url = format!("wss://{}:{}{}", ep.server_name, ep.port, ep.ws_path);
    let (ws, _resp) = tokio_tungstenite::client_async(url, tls)
        .await
        .map_err(ws_err)?;
    Ok(split(ws))
}

pub struct WsListener {
    inner: TcpListener,
    /// `Some` → terminate TLS here (WSS). `None` → plain WS (TLS terminated by a
    /// front proxy such as nginx; bind this on localhost).
    acceptor: Option<TlsAcceptor>,
}

impl WsListener {
    pub async fn bind(addr: SocketAddr, sec: Arc<ServerSecurity>, tls: bool) -> Result<Self> {
        let acceptor = if tls {
            let cfg = tls::server_config(&sec.tls_cert_pem, &sec.tls_key_pem)?;
            Some(TlsAcceptor::from(cfg))
        } else {
            None
        };
        Ok(Self {
            inner: TcpListener::bind(addr).await?,
            acceptor,
        })
    }

    pub async fn accept(&self) -> Result<Accepted> {
        let (tcp, peer_addr) = self.inner.accept().await?;
        tcp.set_nodelay(true).ok();
        let (sink, stream) = match &self.acceptor {
            Some(acceptor) => {
                let tls = acceptor
                    .accept(tcp)
                    .await
                    .map_err(|e| Error::Crypto(format!("tls accept: {e}")))?;
                let ws = tokio_tungstenite::accept_async(tls).await.map_err(ws_err)?;
                split(ws)
            }
            None => {
                let ws = tokio_tungstenite::accept_async(tcp).await.map_err(ws_err)?;
                split(ws)
            }
        };
        Ok(Accepted {
            peer_addr,
            sink,
            stream,
        })
    }
}
