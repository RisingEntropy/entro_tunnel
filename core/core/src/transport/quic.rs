//! QUIC transport (native TLS 1.3 via `quinn`).
//!
//! A single bi-directional stream carries the control+data frames, length-
//! delimited (`u32` prefix) on the already-encrypted QUIC stream. The
//! `quinn::Endpoint` + `Connection` are kept alive by a shared `Keep` held by
//! both channel halves (dropping them would close the connection).

use super::{Accepted, ClientSecurity, Endpoint, MessageSink, MessageStream, ServerSecurity};
use crate::crypto::tls;
use crate::{Error, Result, MAX_MESSAGE_LEN};
use async_trait::async_trait;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Keeps the endpoint + connection alive for the lifetime of the channel.
struct Keep {
    _endpoint: quinn::Endpoint,
    _conn: quinn::Connection,
}

struct QuicSink {
    send: quinn::SendStream,
    _keep: Arc<Keep>,
}

#[async_trait]
impl MessageSink for QuicSink {
    async fn send(&mut self, msg: &[u8]) -> Result<()> {
        if msg.len() > MAX_MESSAGE_LEN {
            return Err(Error::Protocol(format!("message too large: {}", msg.len())));
        }
        // Length prefix via the tokio AsyncWriteExt helper, then quinn's own
        // `write_all` (it shadows the trait method and returns `WriteError`).
        self.send.write_u32(msg.len() as u32).await?;
        self.send
            .write_all(msg)
            .await
            .map_err(|e| Error::Transport(format!("quic write: {e}")))?;
        Ok(())
    }
    async fn close(&mut self) -> Result<()> {
        let _ = self.send.finish();
        Ok(())
    }
}

struct QuicRecv {
    recv: quinn::RecvStream,
    _keep: Arc<Keep>,
}

#[async_trait]
impl MessageStream for QuicRecv {
    async fn recv(&mut self) -> Result<Vec<u8>> {
        let len = match self.recv.read_u32().await {
            Ok(l) => l as usize,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Err(Error::Closed),
            Err(e) => return Err(e.into()),
        };
        if len > MAX_MESSAGE_LEN {
            return Err(Error::Protocol(format!("frame too large: {len}")));
        }
        let mut buf = vec![0u8; len];
        self.recv
            .read_exact(&mut buf)
            .await
            .map_err(|e| Error::Transport(format!("quic read: {e}")))?;
        Ok(buf)
    }
}

fn wrap(
    endpoint: quinn::Endpoint,
    conn: quinn::Connection,
    send: quinn::SendStream,
    recv: quinn::RecvStream,
) -> (Box<dyn MessageSink>, Box<dyn MessageStream>) {
    let keep = Arc::new(Keep {
        _endpoint: endpoint,
        _conn: conn,
    });
    (
        Box::new(QuicSink {
            send,
            _keep: keep.clone(),
        }),
        Box::new(QuicRecv { recv, _keep: keep }),
    )
}

pub async fn connect(
    ep: &Endpoint,
    sec: &ClientSecurity,
) -> Result<(Box<dyn MessageSink>, Box<dyn MessageStream>)> {
    let crypto = tls::quic_client_config(sec.tls_skip_verify, sec.tls_pinned_cert_pem.as_deref())?;
    let qcc = quinn::crypto::rustls::QuicClientConfig::try_from(crypto)
        .map_err(|e| Error::Crypto(format!("quic client crypto: {e}")))?;
    let client_cfg = quinn::ClientConfig::new(Arc::new(qcc));

    let mut endpoint = quinn::Endpoint::client("0.0.0.0:0".parse().unwrap())
        .map_err(|e| Error::Transport(format!("quic endpoint: {e}")))?;
    endpoint.set_default_client_config(client_cfg);

    let addr = tokio::net::lookup_host((ep.host.as_str(), ep.port))
        .await?
        .next()
        .ok_or_else(|| Error::Config(format!("cannot resolve {}", ep.host)))?;

    let conn = endpoint
        .connect(addr, &ep.server_name)
        .map_err(|e| Error::Transport(format!("quic connect: {e}")))?
        .await
        .map_err(|e| Error::Transport(format!("quic handshake: {e}")))?;

    let (send, recv) = conn
        .open_bi()
        .await
        .map_err(|e| Error::Transport(format!("quic open_bi: {e}")))?;

    Ok(wrap(endpoint, conn, send, recv))
}

pub struct QuicListener {
    endpoint: quinn::Endpoint,
}

impl QuicListener {
    pub async fn bind(addr: SocketAddr, sec: Arc<ServerSecurity>) -> Result<Self> {
        let crypto = tls::quic_server_config(&sec.tls_cert_pem, &sec.tls_key_pem)?;
        let qsc = quinn::crypto::rustls::QuicServerConfig::try_from(crypto)
            .map_err(|e| Error::Crypto(format!("quic server crypto: {e}")))?;
        let server_cfg = quinn::ServerConfig::with_crypto(Arc::new(qsc));
        let endpoint = quinn::Endpoint::server(server_cfg, addr)
            .map_err(|e| Error::Transport(format!("quic bind: {e}")))?;
        Ok(Self { endpoint })
    }

    pub async fn accept(&self) -> Result<Accepted> {
        let incoming = self
            .endpoint
            .accept()
            .await
            .ok_or_else(|| Error::Transport("quic endpoint closed".into()))?;
        let conn = incoming
            .await
            .map_err(|e| Error::Transport(format!("quic accept: {e}")))?;
        let peer_addr = conn.remote_address();
        let (send, recv) = conn
            .accept_bi()
            .await
            .map_err(|e| Error::Transport(format!("quic accept_bi: {e}")))?;
        let (sink, stream) = wrap(self.endpoint.clone(), conn, send, recv);
        Ok(Accepted {
            peer_addr,
            sink,
            stream,
        })
    }
}
