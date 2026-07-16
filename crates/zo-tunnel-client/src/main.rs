use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command as ProcCommand, Stdio};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

mod client;
mod config;

#[derive(Parser, Debug)]
#[command(
    name = "zotunnel",
    about = "zotunnel — expose a local HTTP port through your Zo Tunnel server",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Manage client configuration
    Config(ConfigCmd),
    /// Forward a local HTTP port (ngrok-style)
    Http(HttpArgs),
    /// Show config and running background tunnels
    Status,
    /// Stop a background tunnel (or all)
    Stop(StopArgs),
    /// Upgrade to the latest release
    Upgrade,
    /// Uninstall zotunnel
    Uninstall(UninstallArgs),
}

#[derive(Parser, Debug)]
struct ConfigCmd {
    #[command(subcommand)]
    action: ConfigAction,
}

#[derive(Subcommand, Debug)]
enum ConfigAction {
    /// Set server address and auth token
    Set(ConfigSetArgs),
    /// Print current config
    Show,
}

#[derive(Parser, Debug)]
struct ConfigSetArgs {
    #[arg(long)]
    server: String,
    #[arg(long)]
    token: String,
    #[arg(long)]
    tls: bool,
    #[arg(long, default_value = "")]
    tls_server_name: String,
    #[arg(long)]
    tls_skip_verify: bool,
}

#[derive(Parser, Debug, Clone)]
struct HttpArgs {
    /// Local port or host:port (e.g. 3000 or localhost:8080)
    addr: String,
    /// Subdomain name (default: random)
    #[arg(long, short)]
    name: Option<String>,
    /// Run in background and exit when connected
    #[arg(long, short = 'd')]
    detach: bool,
    /// Internal: child process after detach
    #[arg(long, hide = true)]
    daemon_child: bool,
}

#[derive(Parser, Debug)]
struct StopArgs {
    /// Tunnel name (omit to stop all background tunnels)
    name: Option<String>,
}

#[derive(Parser, Debug)]
struct UninstallArgs {
    #[arg(long, short)]
    yes: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Config(cmd) => cmd_config(cmd),
        Command::Http(args) => cmd_http(args).await,
        Command::Status => cmd_status(),
        Command::Stop(args) => cmd_stop(args),
        Command::Upgrade => {
            zo_tunnel_protocol::self_update::upgrade("zotunnel", env!("CARGO_PKG_VERSION"))
        }
        Command::Uninstall(args) => zo_tunnel_protocol::self_update::uninstall(
            "zotunnel",
            zo_tunnel_protocol::self_update::Component::Client,
            args.yes,
            false,
        ),
    }
}

fn tunnels_dir() -> Result<PathBuf> {
    let dir = config::config_dir()?.join("tunnels");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn pid_path(name: &str) -> Result<PathBuf> {
    Ok(tunnels_dir()?.join(format!("{}.pid", name)))
}

fn log_path(name: &str) -> Result<PathBuf> {
    Ok(tunnels_dir()?.join(format!("{}.log", name)))
}

fn is_pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

fn read_pid(name: &str) -> Result<Option<u32>> {
    let path = pid_path(name)?;
    if !path.exists() {
        return Ok(None);
    }
    let s = std::fs::read_to_string(&path)?;
    let pid: u32 = s.trim().parse().unwrap_or(0);
    if pid == 0 {
        return Ok(None);
    }
    if is_pid_alive(pid) {
        Ok(Some(pid))
    } else {
        let _ = std::fs::remove_file(&path);
        Ok(None)
    }
}

fn write_pid(name: &str, pid: u32) -> Result<()> {
    std::fs::write(pid_path(name)?, format!("{}\n", pid))?;
    Ok(())
}

fn list_running_tunnels() -> Result<Vec<(String, u32)>> {
    let dir = tunnels_dir()?;
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(stem) = name.strip_suffix(".pid") {
            if let Some(pid) = read_pid(stem)? {
                out.push((stem.to_string(), pid));
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

fn cmd_config(cmd: ConfigCmd) -> Result<()> {
    match cmd.action {
        ConfigAction::Set(args) => {
            let cfg = config::ZotunnelConfig {
                server: args.server,
                token: args.token,
                tls: config::ClientTlsConfig {
                    enabled: args.tls,
                    server_name: args.tls_server_name,
                    skip_verify: args.tls_skip_verify,
                },
            };
            cfg.save()?;
            println!("Saved {}", config::config_path()?.display());
            println!("  server: {}", cfg.server);
            println!("  token:  {}", cfg.masked_token());
            Ok(())
        }
        ConfigAction::Show => {
            let cfg = config::ZotunnelConfig::load()?;
            println!("server: {}", cfg.server);
            println!("token:  {}", cfg.masked_token());
            println!("tls:    {}", cfg.tls.enabled);
            Ok(())
        }
    }
}

fn cmd_status() -> Result<()> {
    match config::ZotunnelConfig::load() {
        Ok(cfg) => {
            println!("zotunnel v{}", env!("CARGO_PKG_VERSION"));
            println!("server: {}", cfg.server);
            println!("token:  {}", cfg.masked_token());
        }
        Err(_) => {
            println!("zotunnel v{}", env!("CARGO_PKG_VERSION"));
            println!("Not configured. Run: zotunnel config set --server HOST:6200 --token TOKEN");
        }
    }

    let running = list_running_tunnels()?;
    if running.is_empty() {
        println!("tunnels: (none running in background)");
    } else {
        println!("tunnels:");
        for (name, pid) in running {
            println!("  {}  pid={}", name, pid);
        }
        println!("Stop: zotunnel stop <name>   or   zotunnel stop");
    }
    Ok(())
}

fn cmd_stop(args: StopArgs) -> Result<()> {
    let targets: Vec<String> = if let Some(name) = args.name {
        vec![name]
    } else {
        list_running_tunnels()?
            .into_iter()
            .map(|(n, _)| n)
            .collect()
    };

    if targets.is_empty() {
        println!("No background tunnels running.");
        return Ok(());
    }

    for name in targets {
        match read_pid(&name)? {
            Some(pid) => {
                #[cfg(unix)]
                unsafe {
                    libc::kill(pid as i32, libc::SIGTERM);
                }
                let _ = std::fs::remove_file(pid_path(&name)?);
                println!("Stopped '{}' (pid {})", name, pid);
            }
            None => println!("'{}' is not running", name),
        }
    }
    Ok(())
}

fn spawn_detached(args: &HttpArgs, client_id: &str) -> Result<()> {
    if read_pid(client_id)?.is_some() {
        anyhow::bail!(
            "Tunnel '{}' is already running in background. Stop it with: zotunnel stop {}",
            client_id,
            client_id
        );
    }

    let log = log_path(client_id)?;
    let log_file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&log)
        .with_context(|| format!("open log {}", log.display()))?;
    let log_err = log_file.try_clone()?;

    let exe = std::env::current_exe().context("current_exe")?;
    let mut cmd = ProcCommand::new(exe);
    cmd.arg("http").arg(&args.addr).arg("--name").arg(client_id);
    cmd.arg("--daemon-child");
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::from(log_file));
    cmd.stderr(Stdio::from(log_err));

    #[cfg(unix)]
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let child = cmd.spawn().context("spawn background zotunnel")?;
    let pid = child.id();
    write_pid(client_id, pid)?;

    // Wait until connected (Forwarding line) or timeout
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let mut offset = 0u64;
    let mut forwarding = None::<String>;
    while std::time::Instant::now() < deadline {
        if !is_pid_alive(pid) {
            let log_text = std::fs::read_to_string(log_path(client_id)?).unwrap_or_default();
            anyhow::bail!(
                "Background tunnel exited early. Log:\n{}",
                log_text.trim()
            );
        }
        if let Ok(mut f) = std::fs::File::open(log_path(client_id)?) {
            let _ = f.seek(SeekFrom::Start(offset));
            let mut buf = String::new();
            let _ = f.read_to_string(&mut buf);
            offset += buf.len() as u64;
            for line in buf.lines() {
                if let Some(rest) = line.strip_prefix("Forwarding  ") {
                    forwarding = Some(rest.to_string());
                    break;
                }
            }
        }
        if forwarding.is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }

    println!("zotunnel v{}", env!("CARGO_PKG_VERSION"));
    if let Some(fwd) = forwarding {
        println!("Forwarding  {}", fwd);
    } else {
        println!(
            "Started '{}' in background (pid {}). Still connecting…",
            client_id, pid
        );
    }
    println!("Log:    {}", log_path(client_id)?.display());
    println!("Stop:   zotunnel stop {}", client_id);
    println!("Status: zotunnel status");
    Ok(())
}

async fn cmd_http(args: HttpArgs) -> Result<()> {
    let cfg = config::ZotunnelConfig::load()?;
    let local_addr = config::normalize_local_addr(&args.addr);
    let client_id = args
        .name
        .clone()
        .unwrap_or_else(config::generate_name);

    if args.detach && !args.daemon_child {
        return spawn_detached(&args, &client_id);
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .with_target(false)
        .init();

    let client = client::Client::new(
        cfg.server.clone(),
        local_addr.clone(),
        client_id.clone(),
        cfg.token.clone(),
        cfg.tls.clone(),
    );

    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        cancel_clone.cancel();
    });

    let (status_tx, mut status_rx) = watch::channel(client::TunnelStatus::Connecting);
    let mut printed = false;
    let quiet = args.daemon_child;

    if !quiet {
        println!("zotunnel v{}", env!("CARGO_PKG_VERSION"));
        println!("Connecting to {} as '{}'...", cfg.server, client_id);
    } else {
        println!("zotunnel v{}", env!("CARGO_PKG_VERSION"));
        println!("Connecting to {} as '{}'...", cfg.server, client_id);
    }

    let printer = tokio::spawn(async move {
        loop {
            if status_rx.changed().await.is_err() {
                break;
            }
            let status = status_rx.borrow().clone();
            if let client::TunnelStatus::Connected { route, .. } = status {
                if !printed {
                    println!();
                    println!("Forwarding  {} -> http://{}", route, local_addr);
                    if !quiet {
                        println!();
                        println!("Press Ctrl+C to stop");
                    }
                    printed = true;
                }
            }
        }
    });

    let result = run_with_reconnect(&client, cancel, status_tx).await;
    printer.abort();
    let _ = std::fs::remove_file(pid_path(&client_id)?);
    result.context("tunnel session")
}

async fn run_with_reconnect(
    client: &client::Client,
    cancel: CancellationToken,
    status_tx: watch::Sender<client::TunnelStatus>,
) -> Result<()> {
    let mut backoff = 1u64;
    loop {
        if cancel.is_cancelled() {
            return Ok(());
        }
        match client
            .run_cancellable(cancel.clone(), status_tx.clone())
            .await
        {
            Ok(()) => {
                if cancel.is_cancelled() {
                    return Ok(());
                }
                eprintln!("Session ended, reconnecting in {}s...", backoff);
            }
            Err(e) => {
                if cancel.is_cancelled() {
                    return Ok(());
                }
                eprintln!("Error: {:#}. Reconnecting in {}s...", e, backoff);
            }
        }
        tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            _ = tokio::time::sleep(std::time::Duration::from_secs(backoff)) => {}
        }
        backoff = (backoff * 2).min(30);
    }
}
