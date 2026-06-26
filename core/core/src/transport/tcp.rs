//! Raw-TCP transport, encrypted with Noise.
//!
//! After the Noise handshake the `TcpStream` is split into read/write halves.
//! Each channel message is exactly one Noise transport message (ciphertext)
//! prefixed by a `u16` length. The shared [`snow::TransportState`] is guarded by
//! a `std::sync::Mutex`; the lock is only ever held for the (synchronous) AEAD
//! transform, never across an `.await`.

use super::{Accepted, ClientSecurity, Endpoint, MessageSink, MessageStream, ServerSecurity};
use crate::crypto::noise;
use crate::{Error, Result};
use async_trait::async_trait;
use snow::TransportState;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Max plaintext per Noise transport message: 65535 − 16-byte AEAD tag.
const NOISE_MAX_PLAINTEXT: usize = 65519;

/// Dial `ep` over TCP and run the Noise initiator handshake.
pub async fn connect(
    ep: &Endpoint,
    sec: &ClientSecurity,
) -> Result<(Box<dyn MessageSink>, Box<dyn MessageStream>)> {
    let mut stream = TcpStream::connect((ep.host.as_str(), ep.port)).await?;
    stream.set_nodelay(true).ok();
    let ts = noise::initiator_handshake(&mut stream, &sec.noise_psk).await?;
    Ok(wrap(stream, ts))
}

/// Run the Noise initiator handshake over an already-established byte stream
/// (e.g. a relayed chain carrier), rather than dialing a fresh TCP socket.
pub async fn connect_over<S>(
    mut stream: S,
    sec: &ClientSecurity,
) -> Result<(Box<dyn MessageSink>, Box<dyn MessageStream>)>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let ts = noise::initiator_handshake(&mut stream, &sec.noise_psk).await?;
    Ok(wrap(stream, ts))
}

/// A bound TCP listener that runs the Noise responder handshake per connection.
pub struct TcpNoiseListener {
    inner: TcpListener,
    sec: Arc<ServerSecurity>,
}

impl TcpNoiseListener {
    pub async fn bind(addr: SocketAddr, sec: Arc<ServerSecurity>) -> Result<Self> {
        Ok(Self {
            inner: TcpListener::bind(addr).await?,
            sec,
        })
    }

    /// Pull the next pending TCP connection. Only the (fast) kernel accept runs
    /// here — the Noise handshake is deferred to [`TcpIncoming::finish`] so it
    /// never blocks the accept loop.
    pub async fn accept(&self) -> Result<TcpIncoming> {
        let (stream, peer_addr) = self.inner.accept().await?;
        stream.set_nodelay(true).ok();
        Ok(TcpIncoming {
            stream,
            peer_addr,
            sec: self.sec.clone(),
        })
    }
}

/// A TCP connection accepted but not yet through the Noise responder handshake.
pub struct TcpIncoming {
    stream: TcpStream,
    peer_addr: SocketAddr,
    sec: Arc<ServerSecurity>,
}

impl TcpIncoming {
    /// Run the Noise responder handshake. Drive this off the accept loop, under a
    /// timeout, so a peer that stalls mid-handshake can't wedge the listener.
    pub async fn finish(mut self) -> Result<Accepted> {
        let ts = noise::responder_handshake(&mut self.stream, &self.sec.noise_psk).await?;
        let (sink, recv) = wrap(self.stream, ts);
        Ok(Accepted {
            peer_addr: self.peer_addr,
            sink,
            stream: recv,
        })
    }
}

fn wrap<S>(stream: S, ts: TransportState) -> (Box<dyn MessageSink>, Box<dyn MessageStream>)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (r, w) = tokio::io::split(stream);
    let shared = Arc::new(Mutex::new(ts));
    (
        Box::new(NoiseSink {
            w,
            ts: shared.clone(),
        }),
        Box::new(NoiseStream { r, ts: shared }),
    )
}

struct NoiseSink<W> {
    w: W,
    ts: Arc<Mutex<TransportState>>,
}

#[async_trait]
impl<W: AsyncWrite + Unpin + Send> MessageSink for NoiseSink<W> {
    async fn send(&mut self, msg: &[u8]) -> Result<()> {
        if msg.len() > NOISE_MAX_PLAINTEXT {
            return Err(Error::Protocol(format!(
                "noise message too large: {} (max {NOISE_MAX_PLAINTEXT})",
                msg.len()
            )));
        }
        let mut buf = vec![0u8; msg.len() + 16];
        let n = {
            let mut guard = self.ts.lock().expect("noise tx mutex poisoned");
            guard
                .write_message(msg, &mut buf)
                .map_err(|e| Error::Crypto(format!("noise encrypt: {e}")))?
        };
        self.w.write_u16(n as u16).await?;
        self.w.write_all(&buf[..n]).await?;
        self.w.flush().await?;
        Ok(())
    }

    async fn close(&mut self) -> Result<()> {
        self.w.shutdown().await.ok();
        Ok(())
    }
}

struct NoiseStream<R> {
    r: R,
    ts: Arc<Mutex<TransportState>>,
}

#[async_trait]
impl<R: AsyncRead + Unpin + Send> MessageStream for NoiseStream<R> {
    async fn recv(&mut self) -> Result<Vec<u8>> {
        let len = match self.r.read_u16().await {
            Ok(l) => l as usize,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Err(Error::Closed),
            Err(e) => return Err(e.into()),
        };
        let mut cipher = vec![0u8; len];
        self.r.read_exact(&mut cipher).await?;
        let mut out = vec![0u8; len];
        let n = {
            let mut guard = self.ts.lock().expect("noise rx mutex poisoned");
            guard
                .read_message(&cipher, &mut out)
                .map_err(|e| Error::Crypto(format!("noise decrypt: {e}")))?
        };
        out.truncate(n);
        Ok(out)
    }
}
