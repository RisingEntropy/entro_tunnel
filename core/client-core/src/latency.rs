//! Round-trip latency probe to a server.
//!
//! For TCP/WS transports this is a raw TCP connect to the server's port (one
//! network RTT — and for WS that port is the TLS-terminating front, i.e. the
//! server host). For QUIC there is no plain TCP port, so we time the QUIC
//! handshake. Probes are stateless: they never send `Hello`, so they don't
//! register a session and can't disturb a live connection.

use crate::config::ServerEntry;
use entrotunnel_core::config::{parse_psk, TransportKind};
use entrotunnel_core::transport::{self, ClientSecurity, Endpoint};
use entrotunnel_core::{Error, Result};
use std::time::{Duration, Instant};

/// Measure latency to `server`, giving up after `timeout`.
pub async fn measure_latency(server: &ServerEntry, timeout: Duration) -> Result<Duration> {
    match server.transport {
        TransportKind::Tcp | TransportKind::Ws => {
            let start = Instant::now();
            let conn = tokio::time::timeout(
                timeout,
                tokio::net::TcpStream::connect((server.host.as_str(), server.port)),
            )
            .await
            .map_err(|_| Error::Transport("latency probe timed out".into()))?
            .map_err(|e| Error::Transport(format!("connect: {e}")))?;
            let rtt = start.elapsed();
            drop(conn);
            Ok(rtt)
        }
        TransportKind::Quic => {
            let security = ClientSecurity {
                noise_psk: parse_psk(&server.noise_psk)?,
                tls_skip_verify: server.tls_skip_verify,
                tls_pinned_cert_pem: None,
            };
            let endpoint = Endpoint {
                host: server.host.clone(),
                port: server.port,
                kind: TransportKind::Quic,
                ws_path: "/et".to_string(),
                server_name: server.sni(),
            };
            let start = Instant::now();
            let (mut sink, _stream) =
                tokio::time::timeout(timeout, transport::connect(&endpoint, &security))
                    .await
                    .map_err(|_| Error::Transport("latency probe timed out".into()))??;
            let rtt = start.elapsed();
            let _ = sink.close().await;
            Ok(rtt)
        }
    }
}
