//! Encryption helpers: Noise handshake (raw TCP) and TLS material (WSS/QUIC).

#[cfg(feature = "tcp")]
pub mod noise;

#[cfg(feature = "tls")]
pub mod tls;
