//! `entrotunnel-client` — the shared client *engine*.
//!
//! Both the CLI (`entrotunnel-cli`) and the Tauri GUI link this crate and drive
//! the same code. The GUI never re-implements tunnelling logic; it only renders
//! state and calls [`Engine`].
//!
//! Responsibilities:
//! * [`config::ClientConfig`] — the persisted client profile.
//! * [`tun`] — cross-platform virtual NIC (Linux implemented; macOS/Windows
//!   scaffolded).
//! * [`netcfg`] — OS routing/DNS changes for global-proxy / VPN modes.
//! * [`engine`] — connect → handshake → run the selected mode until cancelled.

pub mod chain;
pub mod config;
pub mod engine;
pub mod latency;
pub mod netcfg;
pub mod proxy;
pub mod sysproxy;

/// The TUN device abstraction lives in `entrotunnel-core` so the server can
/// reuse it; re-exported here for convenience.
pub use entrotunnel_core::tun;

pub use engine::{Engine, EngineHandle};
pub use entrotunnel_core::{Error, Result};
