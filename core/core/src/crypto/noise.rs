//! Noise handshake over a raw TCP stream.
//!
//! Pattern: `Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s`.
//! Ephemeral X25519 keys give forward secrecy; the 32-byte `noise_psk` mixed in
//! at `psk0` authenticates the channel (rejects anyone without the PSK and
//! resists MITM). Per-peer authentication happens later at the `Hello` layer.

use crate::{Error, Result, NOISE_PATTERN};
use snow::TransportState;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

fn noise_err(e: snow::Error) -> Error {
    Error::Crypto(format!("noise: {e}"))
}

async fn write_hs<S: AsyncWrite + Unpin>(stream: &mut S, data: &[u8]) -> Result<()> {
    stream.write_u16(data.len() as u16).await?;
    stream.write_all(data).await?;
    stream.flush().await?;
    Ok(())
}

async fn read_hs<S: AsyncRead + Unpin>(stream: &mut S) -> Result<Vec<u8>> {
    let len = stream.read_u16().await? as usize;
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;
    Ok(buf)
}

/// Run the Noise initiator (client) handshake, returning the transport state.
/// Generic over the byte stream so it works over a raw socket OR a relayed
/// (chain) carrier stream.
pub async fn initiator_handshake<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    psk: &[u8; 32],
) -> Result<TransportState> {
    let params = NOISE_PATTERN
        .parse()
        .map_err(|e| Error::Crypto(format!("noise params: {e:?}")))?;
    let mut hs = snow::Builder::new(params)
        .psk(0, psk)
        .build_initiator()
        .map_err(noise_err)?;

    let mut buf = vec![0u8; 1024];
    // -> e
    let n = hs.write_message(&[], &mut buf).map_err(noise_err)?;
    write_hs(stream, &buf[..n]).await?;
    // <- e, ee
    let msg = read_hs(stream).await?;
    let mut scratch = vec![0u8; 1024];
    hs.read_message(&msg, &mut scratch).map_err(noise_err)?;

    hs.into_transport_mode().map_err(noise_err)
}

/// Run the Noise responder (server) handshake, returning the transport state.
pub async fn responder_handshake<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    psk: &[u8; 32],
) -> Result<TransportState> {
    let params = NOISE_PATTERN
        .parse()
        .map_err(|e| Error::Crypto(format!("noise params: {e:?}")))?;
    let mut hs = snow::Builder::new(params)
        .psk(0, psk)
        .build_responder()
        .map_err(noise_err)?;

    // <- e
    let msg = read_hs(stream).await?;
    let mut scratch = vec![0u8; 1024];
    hs.read_message(&msg, &mut scratch).map_err(noise_err)?;
    // -> e, ee
    let mut buf = vec![0u8; 1024];
    let n = hs.write_message(&[], &mut buf).map_err(noise_err)?;
    write_hs(stream, &buf[..n]).await?;

    hs.into_transport_mode().map_err(noise_err)
}
