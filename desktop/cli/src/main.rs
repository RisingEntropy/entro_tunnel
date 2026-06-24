//! `entrotunnel-cli` — headless client. Shares the `entrotunnel-client` engine
//! with the Tauri GUI; the GUI is just a different front-end over the same core.

use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, Subcommand};
use entrotunnel_client::config::{
    ClientConfig, ConnectionSettings, Profile, RouteRule, ServerEntry, SplitMode,
};
use entrotunnel_client::Engine;
use entrotunnel_core::config::{generate_psk, generate_token, SessionMode, TransportKind};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "entrotunnel-cli", version, about = "EntroTunnel headless client")]
struct Args {
    #[arg(short, long, default_value = "client.toml", global = true)]
    config: PathBuf,

    /// Override the active server by name (otherwise uses `selected_server`).
    #[arg(short, long, global = true)]
    server: Option<String>,

    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Connect and run until Ctrl-C (default).
    Run,
    /// List the configured servers and which one is active.
    Servers,
    /// Measure latency to every configured server.
    Ping,
    /// Import a server-exported `entro://…` link into a `client.toml`.
    Import {
        /// The `entro://…` link copied from the server admin panel.
        link: String,
        /// Session mode to run: global_proxy | http_proxy | vpn.
        #[arg(long, default_value = "global_proxy")]
        mode: String,
    },
    /// Write a starter `client.toml` to the --config path.
    GenConfig {
        /// Server host or IP.
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        #[arg(long, default_value_t = 8443)]
        port: u16,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    match args.cmd.unwrap_or(Cmd::Run) {
        Cmd::Import { link, mode } => {
            let profile = Profile::decode_link(&link).context("decoding config link")?;
            let mode = match mode.as_str() {
                "global_proxy" | "global" => SessionMode::GlobalProxy,
                "system_proxy" | "system" => SessionMode::SystemProxy,
                "http_proxy" | "http" => SessionMode::HttpProxy,
                "vpn" => SessionMode::Vpn,
                other => anyhow::bail!(
                    "unknown mode '{other}' (use global_proxy|system_proxy|http_proxy|vpn)"
                ),
            };
            let settings = ConnectionSettings {
                mode,
                client_name: Some("cli".into()),
                ..Default::default()
            };
            let cfg = ClientConfig::compose(&profile, &settings);
            cfg.save(&args.config)
                .with_context(|| format!("writing {}", args.config.display()))?;
            println!("imported '{}' → {}", profile.name, args.config.display());
            for s in &profile.servers {
                println!("  server {:<16} {}:{}  [{}]", s.name, s.host, s.port, s.transport);
            }
            println!("mode: {mode}");
        }
        Cmd::GenConfig { host, port } => {
            let token = generate_token();
            let psk = generate_psk();
            let cfg = ClientConfig {
                name: "default".into(),
                selected_server: Some("main".into()),
                mode: SessionMode::GlobalProxy,
                requested_ip: None,
                client_name: Some("cli".into()),
                tun_name: "et0".into(),
                http_listen: "127.0.0.1:7890".into(),
                join_vpn: false,
                // legacy single-server fields left empty; using `servers` below.
                server_host: String::new(),
                server_port: 0,
                transport: TransportKind::Tcp,
                token: String::new(),
                noise_psk: String::new(),
                tls_skip_verify: false,
                server_name: None,
                // Add more `[[servers]]` and switch with `selected_server` or
                // `entrotunnel-cli --server <name> run`.
                servers: vec![ServerEntry {
                    name: "main".into(),
                    host,
                    port,
                    transport: TransportKind::Tcp,
                    token,
                    noise_psk: psk,
                    tls_skip_verify: false,
                    server_name: None,
                }],
                // Example split-tunnel rules: keep LAN traffic off the tunnel.
                routes: vec![
                    RouteRule { target: "192.168.0.0/16".into(), via: "direct".into(), gateway: None },
                    RouteRule { target: "10.0.0.0/8".into(), via: "direct".into(), gateway: None },
                ],
                // Blacklist: tunnel everything except the `direct` rules above.
                split_mode: SplitMode::default(),
                // No proxy chain by default (single-hop).
                chain: Vec::new(),
                // IPv6 kill-switch on by default (blocks native v6 leak on v4-only servers).
                ipv6_killswitch: true,
            };
            cfg.save(&args.config)
                .with_context(|| format!("writing {}", args.config.display()))?;
            println!("wrote {}", args.config.display());
            println!("NOTE: each server's token + noise_psk must match a peer record on that server.");
        }
        Cmd::Servers => {
            let cfg = ClientConfig::load(&args.config)
                .with_context(|| format!("loading {}", args.config.display()))?;
            let active = cfg.active_server().ok();
            let active_name = active.as_ref().map(|s| s.name.as_str());
            println!("servers in {}:", args.config.display());
            if cfg.servers.is_empty() {
                println!("  (legacy single-server config — no named servers)");
            }
            for s in &cfg.servers {
                let mark = if Some(s.name.as_str()) == active_name { "*" } else { " " };
                println!("  {mark} {:<16} {}:{}  [{}]", s.name, s.host, s.port, s.transport);
            }
            if let Some(s) = active {
                println!("active: {} ({}:{} via {})", s.name, s.host, s.port, s.transport);
            }
        }
        Cmd::Ping => {
            let cfg = ClientConfig::load(&args.config)
                .with_context(|| format!("loading {}", args.config.display()))?;
            let servers: Vec<ServerEntry> = if cfg.servers.is_empty() {
                cfg.active_server().ok().into_iter().collect()
            } else {
                cfg.servers.clone()
            };
            println!("pinging {} server(s)…", servers.len());
            let mut handles = Vec::new();
            for s in servers {
                handles.push(tokio::spawn(async move {
                    let r = entrotunnel_client::latency::measure_latency(
                        &s,
                        std::time::Duration::from_secs(5),
                    )
                    .await;
                    (s, r)
                }));
            }
            let mut results = Vec::new();
            for h in handles {
                if let Ok(x) = h.await {
                    results.push(x);
                }
            }
            let best = results
                .iter()
                .filter_map(|(_, r)| r.as_ref().ok().map(|d| d.as_millis()))
                .min();
            for (s, r) in &results {
                match r {
                    Ok(d) => {
                        let ms = d.as_millis();
                        let mark = if Some(ms) == best { "  *fastest" } else { "" };
                        println!(
                            "  {:<16} {}:{} [{}]  {} ms{}",
                            s.name, s.host, s.port, s.transport, ms, mark
                        );
                    }
                    Err(e) => println!(
                        "  {:<16} {}:{} [{}]  unreachable ({e})",
                        s.name, s.host, s.port, s.transport
                    ),
                }
            }
        }
        Cmd::Run => {
            let mut cfg = ClientConfig::load(&args.config)
                .with_context(|| format!("loading {}", args.config.display()))?;
            if let Some(name) = args.server {
                cfg.selected_server = Some(name);
            }

            let cancel = CancellationToken::new();
            let signal_cancel = cancel.clone();
            tokio::spawn(async move {
                let _ = tokio::signal::ctrl_c().await;
                tracing::info!("Ctrl-C received, shutting down");
                signal_cancel.cancel();
            });

            Engine::run(cfg, cancel).await.context("engine run")?;
        }
    }
    Ok(())
}
