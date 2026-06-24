//! `entrotunnel-server` — listeners + session router + web admin.

mod config;
mod net;
mod session;
mod web;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use config::ServerConfig;
use entrotunnel_core::config::parse_psk;
use entrotunnel_core::transport::{Listener, ServerSecurity};
use session::{handle_connection, run_tun_router, AppState, Metrics, Router};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "entrotunnel-server", version, about = "EntroTunnel server")]
struct Args {
    #[arg(short, long, default_value = "server.toml", global = true)]
    config: PathBuf,

    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the server (default).
    Run,
    /// Write a starter `server.toml` to the --config path.
    GenConfig,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    match args.cmd.unwrap_or(Cmd::Run) {
        Cmd::GenConfig => {
            let cfg = ServerConfig::template();
            cfg.save(&args.config)
                .with_context(|| format!("writing {}", args.config.display()))?;
            println!("wrote {}", args.config.display());
            println!("noise_psk : {}", cfg.security.noise_psk);
            println!("admin_token: {}", cfg.web.admin_token);
            if let Some(p) = cfg.peers.first() {
                println!("example peer token: {} -> {}", p.token, p.ip);
            }
            Ok(())
        }
        Cmd::Run => run(args.config).await,
    }
}

async fn run(config_path: PathBuf) -> Result<()> {
    let cfg = ServerConfig::load(&config_path)
        .with_context(|| format!("loading {}", config_path.display()))?;

    let prefix = cfg.prefix_len().context("subnet prefix")?;
    let subnet = cfg.network.subnet.clone();

    // Security material.
    let noise_psk = parse_psk(&cfg.security.noise_psk).context("noise_psk")?;
    let (tls_cert_pem, tls_key_pem) = tls_material(&cfg, &config_path)?;
    let security = Arc::new(ServerSecurity {
        noise_psk,
        tls_cert_pem,
        tls_key_pem,
    });

    // Bring up the server TUN + NAT (Linux). Kept alive for cleanup-on-drop.
    let server_net = match net::setup(&cfg.network, prefix, &subnet).await {
        Ok(n) => Some(n),
        Err(e) => {
            warn!("network setup failed: {e}");
            warn!("→ global-proxy / VPN packet routing is disabled; handshake + web admin still work");
            None
        }
    };
    let tun = server_net.as_ref().map(|n| n.tun.clone());
    let ipv6 = server_net.as_ref().map(|n| n.ipv6).unwrap_or(false);

    let router = Router::new();
    let metrics = Metrics::new();
    let state = AppState {
        config: Arc::new(RwLock::new(cfg.clone())),
        config_path: Arc::new(config_path),
        router: router.clone(),
        tun: tun.clone(),
        metrics: metrics.clone(),
        ipv6,
    };

    // Server-side TUN reader (routes packets to the owning peer session).
    if let Some(t) = tun {
        tokio::spawn(run_tun_router(t, router.clone()));
    }

    // Throughput sampler: snapshots traffic every 5s for the admin timeline.
    {
        let m = metrics.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(5));
            loop {
                tick.tick().await;
                m.sample();
            }
        });
    }

    // One accept loop per configured listener.
    for lc in cfg.listeners.clone() {
        let addr: SocketAddr = lc
            .bind
            .parse()
            .with_context(|| format!("bad listener bind {}", lc.bind))?;
        let sec = security.clone();
        let st = state.clone();
        let kind = lc.transport;
        let ws_tls = lc.tls;
        tokio::spawn(async move {
            match Listener::bind(addr, kind, sec, ws_tls).await {
                Ok(listener) => {
                    info!(%addr, transport = %kind, "listening");
                    loop {
                        match listener.accept().await {
                            Ok(accepted) => {
                                let s = st.clone();
                                tokio::spawn(handle_connection(s, accepted));
                            }
                            Err(e) => tracing::debug!(%addr, "accept/handshake failed: {e}"),
                        }
                    }
                }
                Err(e) => error!(%addr, transport = %kind, "failed to bind: {e}"),
            }
        });
    }

    // Web admin.
    {
        let bind: SocketAddr = cfg
            .web
            .bind
            .parse()
            .with_context(|| format!("bad web bind {}", cfg.web.bind))?;
        let st = state.clone();
        tokio::spawn(async move {
            if let Err(e) = web::serve(st, bind).await {
                error!("web admin failed: {e}");
            }
        });
    }

    info!("entrotunnel-server up; press Ctrl-C / send SIGTERM to stop");
    wait_for_shutdown().await?;
    info!("shutting down (restoring network)");
    drop(server_net); // runs ServerNet::Drop → removes iptables/forwarding
    Ok(())
}

/// Wait for SIGINT (Ctrl-C) or SIGTERM (systemctl stop) so the service shuts
/// down cleanly and restores the network on either.
async fn wait_for_shutdown() -> Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = signal(SignalKind::terminate())?;
        let mut sigint = signal(SignalKind::interrupt())?;
        tokio::select! {
            _ = sigterm.recv() => {}
            _ = sigint.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await?;
    }
    Ok(())
}

#[cfg(feature = "tls")]
fn tls_material(cfg: &ServerConfig, config_path: &std::path::Path) -> Result<(String, String)> {
    use entrotunnel_core::crypto::tls::generate_self_signed;

    if let (Some(cert), Some(key)) = (&cfg.security.tls_cert_path, &cfg.security.tls_key_path) {
        let c = std::fs::read_to_string(cert).with_context(|| format!("read cert {cert}"))?;
        let k = std::fs::read_to_string(key).with_context(|| format!("read key {key}"))?;
        return Ok((c, k));
    }

    let needs_tls = cfg
        .listeners
        .iter()
        .any(|l| !matches!(l.transport, entrotunnel_core::config::TransportKind::Tcp));
    if !needs_tls {
        return Ok((String::new(), String::new()));
    }

    // Generate + persist a self-signed pair next to the config.
    let (cert_pem, key_pem) =
        generate_self_signed(vec!["entrotunnel".into(), "localhost".into()])?;
    let dir = config_path.parent().unwrap_or(std::path::Path::new("."));
    let _ = std::fs::write(dir.join("entrotunnel-cert.pem"), &cert_pem);
    let _ = std::fs::write(dir.join("entrotunnel-key.pem"), &key_pem);
    info!("generated self-signed TLS certificate (entrotunnel-cert.pem)");
    Ok((cert_pem, key_pem))
}

#[cfg(not(feature = "tls"))]
fn tls_material(_cfg: &ServerConfig, _config_path: &std::path::Path) -> Result<(String, String)> {
    Ok((String::new(), String::new()))
}
