use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

mod config;
mod dashboard;
mod metrics;
mod proxy;
mod registry;
mod server;
mod traefik;

#[derive(Parser, Debug)]
#[command(
    name = "zo-tunnel-server",
    about = "Zo Tunnel server — run via Docker Compose with Traefik",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Start the tunnel server in foreground (Docker).
    Start(StartArgs),
}

#[derive(Parser, Debug)]
struct StartArgs {
    #[arg(long, env = "ZO_DOMAIN")]
    domain: Option<String>,

    #[arg(long, env = "ZO_TOKEN")]
    token: Option<String>,

    #[arg(long, env = "ZO_DASHBOARD_TOKEN")]
    dashboard_token: Option<String>,

    #[arg(long, env = "ZO_CONTROL_PORT", default_value_t = 6200)]
    control_port: u16,

    #[arg(long, env = "ZO_PUBLIC_PORT", default_value_t = 6210)]
    public_port: u16,

    #[arg(long, env = "ZO_CONFIG")]
    config: Option<String>,

    #[arg(long)]
    force: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Start(args) => run_server(args).await,
    }
}

async fn run_server(args: StartArgs) -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config_path = args
        .config
        .as_ref()
        .map(std::path::PathBuf::from)
        .or_else(config::ServerConfig::resolve_config_path)
        .unwrap_or_else(config::ServerConfig::config_path);

    let mut cfg = if config_path.exists() && !args.force {
        config::ServerConfig::load(&config_path).context("load config")?
    } else {
        let domain = args.domain.clone().unwrap_or_else(|| {
            eprintln!("❌ --domain (or ZO_DOMAIN) is required on first run");
            std::process::exit(1);
        });
        let cfg = build_config(&args, &domain);
        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let yaml = serde_yaml::to_string(&cfg)?;
        std::fs::write(
            &config_path,
            format!(
                "# Zo Tunnel Server Configuration\n# Config path: {}\n\n{}",
                config_path.display(),
                yaml
            ),
        )?;
        tracing::info!("Config written to {}", config_path.display());
        cfg
    };

    apply_env_overrides(&mut cfg);

    tracing::info!(
        "Zo Tunnel Server v{} | Domain:*.{} | Control:{} | Public:{} | Traefik:{}",
        env!("CARGO_PKG_VERSION"),
        cfg.domain,
        cfg.control_port,
        cfg.public_port,
        cfg.traefik.enabled,
    );
    tracing::info!("Config: {}", config_path.display());
    tracing::info!("Install: https://{}/install", cfg.domain);
    if let Some(token) = cfg.auth.tokens.first() {
        tracing::info!("Client token: {}", token);
    }

    server::Server::new(cfg)
        .run()
        .await
        .context("server run failed")?;
    Ok(())
}

fn build_config(args: &StartArgs, domain: &str) -> config::ServerConfig {
    let client_token = args
        .token
        .as_ref()
        .filter(|t| !t.is_empty())
        .cloned()
        .unwrap_or_else(|| config::ServerConfig::generate_token(24));
    let dashboard_token = args
        .dashboard_token
        .as_ref()
        .filter(|t| !t.is_empty())
        .cloned()
        .unwrap_or_else(|| config::ServerConfig::generate_token(16));

    let mut cfg = config::ServerConfig {
        domain: domain.to_string(),
        control_port: args.control_port,
        public_port: args.public_port,
        ..Default::default()
    };
    cfg.auth.tokens = vec![client_token];
    cfg.dashboard_auth.token = dashboard_token;
    cfg.traefik = traefik::TraefikConfig::auto_detect();
    cfg
}

fn apply_env_overrides(cfg: &mut config::ServerConfig) {
    if let Ok(dir) = std::env::var("ZO_TRAEFIK_CONFIG_DIR") {
        cfg.traefik.enabled = true;
        cfg.traefik.config_dir = dir;
    }
    if let Ok(url) = std::env::var("ZO_TRAEFIK_SERVICE_URL") {
        cfg.traefik.service_url = url;
    }
    if std::env::var("ZO_TRAEFIK_ENABLED").ok().as_deref() == Some("true") {
        cfg.traefik.enabled = true;
    }
    if let Ok(dir) = std::env::var("ZO_CLIENTS_DIR") {
        cfg.clients_dir = dir;
    }
    // Env tokens always win when set (so .env changes apply without --force)
    if let Ok(token) = std::env::var("ZO_TOKEN") {
        if !token.is_empty() {
            cfg.auth.tokens = vec![token];
        }
    }
    if let Ok(token) = std::env::var("ZO_DASHBOARD_TOKEN") {
        if !token.is_empty() {
            cfg.dashboard_auth.token = token;
        }
    }
}
