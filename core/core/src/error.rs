//! Crate-wide error type.

/// The single error type returned across `entrotunnel-core`.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("codec: {0}")]
    Codec(#[from] bincode::Error),

    #[error("crypto: {0}")]
    Crypto(String),

    #[error("protocol: {0}")]
    Protocol(String),

    #[error("auth: {0}")]
    Auth(String),

    #[error("transport: {0}")]
    Transport(String),

    #[error("config: {0}")]
    Config(String),

    /// The peer closed the channel cleanly (EOF). Loops treat this as "stop".
    #[error("connection closed")]
    Closed,

    /// A feature/transport that is scaffolded but not yet wired up.
    #[error("not implemented: {0}")]
    NotImplemented(&'static str),
}

pub type Result<T> = std::result::Result<T, Error>;
