//! Generic length-delimited framing over any `AsyncRead`/`AsyncWrite` half.
//!
//! Reused by the TLS-over-TCP and QUIC transports, where the underlying bytes
//! are already encrypted (by TLS) and we only need message boundaries.

use super::{MessageSink, MessageStream};
use crate::{Error, Result, MAX_MESSAGE_LEN};
use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Length-delimited sink: `u32` big-endian length prefix + payload.
pub struct LdSink<W> {
    w: W,
}

impl<W> LdSink<W> {
    pub fn new(w: W) -> Self {
        Self { w }
    }
}

#[async_trait]
impl<W: AsyncWrite + Unpin + Send> MessageSink for LdSink<W> {
    async fn send(&mut self, msg: &[u8]) -> Result<()> {
        if msg.len() > MAX_MESSAGE_LEN {
            return Err(Error::Protocol(format!("message too large: {}", msg.len())));
        }
        self.w.write_u32(msg.len() as u32).await?;
        self.w.write_all(msg).await?;
        self.w.flush().await?;
        Ok(())
    }

    async fn close(&mut self) -> Result<()> {
        self.w.shutdown().await?;
        Ok(())
    }
}

/// Length-delimited stream paired with [`LdSink`].
pub struct LdStream<R> {
    r: R,
}

impl<R> LdStream<R> {
    pub fn new(r: R) -> Self {
        Self { r }
    }
}

#[async_trait]
impl<R: AsyncRead + Unpin + Send> MessageStream for LdStream<R> {
    async fn recv(&mut self) -> Result<Vec<u8>> {
        let len = match self.r.read_u32().await {
            Ok(l) => l as usize,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Err(Error::Closed),
            Err(e) => return Err(e.into()),
        };
        if len > MAX_MESSAGE_LEN {
            return Err(Error::Protocol(format!("frame too large: {len}")));
        }
        let mut buf = vec![0u8; len];
        self.r.read_exact(&mut buf).await?;
        Ok(buf)
    }
}
