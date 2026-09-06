//! Core server — control channel, public HTTP proxy, dashboard, TCP tunnels.

use crate::config::ServerConfig;
use crate::dashboard::{self, DashboardState};
use crate::metrics::{Metrics, RateLimiter};
use crate::proxy;

use crate::registry::Registry;
use anyhow::{Context, Result};
use http_body_util::BodyExt;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use std::future::poll_fn;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
use tokio_util::compat::TokioAsyncReadCompatExt;
use zo_tunnel_protocol::*;

/// Command sent to the yamux driver task.
pub enum YamuxCmd {
    OpenStream {
        reply: oneshot::Sender<anyhow::Result<yamux::Stream>>,
    },
}

/// Handle to interact with the yamux driver — send commands to open streams.
#[derive(Clone)]
pub struct YamuxHandle {
    cmd_tx: mpsc::Sender<YamuxCmd>,
    abort: tokio::task::AbortHandle,
}

impl YamuxHandle {
    pub async fn open_stream(&self) -> anyhow::Result<yamux::Stream> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.cmd_tx
            .send(YamuxCmd::OpenStream { reply: reply_tx })
            .await
            .map_err(|_| anyhow::anyhow!("yamux driver gone"))?;
        reply_rx
            .await
            .map_err(|_| anyhow::anyhow!("yamux driver dropped"))?
    }

    pub fn abort(&self) {
        self.abort.abort();
    }

    pub fn is_closed(&self) -> bool {
        self.cmd_tx.is_closed()
    }
}

#[cfg(test)]
impl YamuxHandle {
    pub fn mock() -> (Self, mpsc::Receiver<YamuxCmd>) {
        let (cmd_tx, cmd_rx) = mpsc::channel(1);
        let task = tokio::spawn(std::future::pending::<()>());
        (
            Self {
                cmd_tx,
                abort: task.abort_handle(),
            },
            cmd_rx,
        )
    }

    pub fn mock_closed() -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel(1);
        drop(cmd_rx);
        let task = tokio::spawn(async {});
        Self {
            cmd_tx,
            abort: task.abort_handle(),
        }
    }
}

enum DriveEvent {
    Inbound(Option<Result<yamux::Stream, yamux::ConnectionError>>),
    Outbound(Result<yamux::Stream, yamux::ConnectionError>),
}

/// Spawns a task that drives the yamux connection.
/// Returns a handle for opening outbound streams.
fn spawn_yamux_driver<S>(
    stream: S,
    mode: yamux::Mode,
    client_id: String,
    supports_heartbeat: bool,
    last_activity: Arc<std::sync::Mutex<std::time::Instant>>,
    missed_heartbeats: Arc<std::sync::atomic::AtomicU32>,
) -> (YamuxHandle, tokio::task::JoinHandle<()>)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<YamuxCmd>(64);

    let task = tokio::spawn(async move {
        let compat = stream.compat();
        let cfg = yamux::Config::default();
        let mut conn = yamux::Connection::new(compat, cfg, mode);

        let mut heartbeat_interval =
            tokio::time::interval(Duration::from_secs(HEARTBEAT_INTERVAL_SECS));
        heartbeat_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        heartbeat_interval.tick().await;
        const MAX_MISSED: u32 = 5;

        let mut pending_open: Option<oneshot::Sender<anyhow::Result<yamux::Stream>>> = None;
        let mut want_hb_stream = false;
        let mut hb_in_flight = false;
        let (hb_done_tx, mut hb_done_rx) = mpsc::channel::<Result<(), anyhow::Error>>(8);

        loop {
            let want_outbound = pending_open.is_some() || want_hb_stream;

            tokio::select! {
                event = poll_fn(|cx| {
                    match conn.poll_next_inbound(cx) {
                        Poll::Ready(v) => return Poll::Ready(DriveEvent::Inbound(v)),
                        Poll::Pending => {}
                    }
                    if want_outbound {
                        match conn.poll_new_outbound(cx) {
                            Poll::Ready(v) => return Poll::Ready(DriveEvent::Outbound(v)),
                            Poll::Pending => {}
                        }
                    }
                    Poll::Pending
                }) => {
                    match event {
                        DriveEvent::Inbound(result) => match result {
                            Some(Ok(_stream)) => {
                                tracing::debug!("Unexpected inbound stream from '{}'", client_id);
                            }
                            Some(Err(e)) => {
                                tracing::debug!("Yamux error for '{}': {}", client_id, e);
                                break;
                            }
                            None => {
                                tracing::info!("Yamux connection closed for '{}'", client_id);
                                break;
                            }
                        },
                        DriveEvent::Outbound(result) => match result {
                            Ok(stream) => {
                                if let Some(reply) = pending_open.take() {
                                    let _ = reply.send(Ok(stream));
                                } else if want_hb_stream {
                                    want_hb_stream = false;
                                    hb_in_flight = true;
                                    let tx = hb_done_tx.clone();
                                    tokio::spawn(async move {
                                        use futures::io::{AsyncReadExt, AsyncWriteExt};
                                        let result = tokio::time::timeout(
                                            Duration::from_secs(10),
                                            async {
                                                let mut stream = stream;
                                                stream.write_all(&[STREAM_TYPE_HEARTBEAT]).await?;
                                                let mut pong = [0u8; 1];
                                                stream.read_exact(&mut pong).await?;
                                                stream.close().await?;
                                                Ok::<(), anyhow::Error>(())
                                            },
                                        )
                                        .await;
                                        let mapped = match result {
                                            Ok(Ok(())) => Ok(()),
                                            Ok(Err(e)) => Err(e),
                                            Err(_) => Err(anyhow::anyhow!("heartbeat timeout")),
                                        };
                                        let _ = tx.send(mapped).await;
                                    });
                                }
                            }
                            Err(e) => {
                                if let Some(reply) = pending_open.take() {
                                    let _ = reply.send(Err(anyhow::anyhow!("yamux open: {}", e)));
                                }
                                if want_hb_stream {
                                    want_hb_stream = false;
                                    let n = missed_heartbeats.fetch_add(1, Ordering::Relaxed) + 1;
                                    tracing::warn!(
                                        "💔 Heartbeat open failed for '{}': {} ({}/{})",
                                        client_id,
                                        e,
                                        n,
                                        MAX_MISSED
                                    );
                                    if n >= MAX_MISSED {
                                        tracing::error!(
                                            "💀 Connection dead for '{}' — {} missed heartbeats, disconnecting",
                                            client_id,
                                            n
                                        );
                                        break;
                                    }
                                }
                            }
                        },
                    }
                }
                cmd = cmd_rx.recv(), if pending_open.is_none() => {
                    match cmd {
                        Some(YamuxCmd::OpenStream { reply }) => {
                            pending_open = Some(reply);
                        }
                        None => {
                            tracing::debug!("All yamux handles dropped for '{}'", client_id);
                            break;
                        }
                    }
                }
                _ = heartbeat_interval.tick(), if supports_heartbeat && !hb_in_flight && !want_hb_stream => {
                    want_hb_stream = true;
                }
                done = hb_done_rx.recv() => {
                    hb_in_flight = false;
                    match done {
                        Some(Ok(())) => {
                            missed_heartbeats.store(0, Ordering::Relaxed);
                            if let Ok(mut ts) = last_activity.lock() {
                                *ts = std::time::Instant::now();
                            }
                            tracing::trace!("💓 Heartbeat OK for '{}'", client_id);
                        }
                        Some(Err(e)) => {
                            let n = missed_heartbeats.fetch_add(1, Ordering::Relaxed) + 1;
                            tracing::warn!(
                                "💔 Heartbeat failed for '{}': {} ({}/{})",
                                client_id,
                                e,
                                n,
                                MAX_MISSED
                            );
                            if n >= MAX_MISSED {
                                tracing::error!(
                                    "💀 Connection dead for '{}' — {} missed heartbeats, disconnecting",
                                    client_id,
                                    n
                                );
                                break;
                            }
                        }
                        None => break,
                    }
                }
            }
        }
    });

    let handle = YamuxHandle {
        cmd_tx,
        abort: task.abort_handle(),
    };
    (handle, task)
}

/// Core Zo Tunnel server.
pub struct Server {
    config: ServerConfig,
}

impl Server {
    pub fn new(config: ServerConfig) -> Self {
        Self { config }
    }

    pub async fn run(&self) -> Result<()> {
        let registry = Arc::new(Registry::new());
        let metrics = Arc::new(Metrics::new());
        let rate_limiter = Arc::new(RateLimiter::new(self.config.rate_limit.requests_per_second));

        // ── Validate domain ──
        if self.config.domain.is_empty() {
            anyhow::bail!(
                "domain is required — run `zo-tunnel-server setup --domain <domain>` first"
            );
        }
        let domain = &self.config.domain;

        // ── Bind control port ──
        let control_listener = TcpListener::bind(("0.0.0.0", self.config.control_port))
            .await
            .with_context(|| format!("bind control port {}", self.config.control_port))?;
        tracing::info!("🔌 Control channel on :{}", self.config.control_port);

        // ── Traefik dynamic routes ──
        let traefik_manager = if self.config.traefik.enabled {
            let mgr = Arc::new(crate::traefik::TraefikManager::new(
                self.config.traefik.clone(),
                domain.clone(),
            ));
            if let Err(e) = mgr.write_static_routes() {
                tracing::warn!("⚠️  Traefik static routes failed: {:#}", e);
            } else {
                tracing::info!(
                    "🔀 Traefik enabled — dynamic dir: {}",
                    self.config.traefik.config_dir
                );
            }
            Some(mgr)
        } else {
            None
        };

        // ── Dashboard state ──
        let dash_state = DashboardState {
            registry: registry.clone(),
            metrics: metrics.clone(),
            dashboard_token: self.config.dashboard_auth.token.clone(),
            client_token: self.config.auth.tokens.first().cloned().unwrap_or_default(),
            auth_enabled: self.config.dashboard_auth_enabled(),
            tls_enabled: self.config.traefik.enabled,
            domain: domain.clone(),
            control_port: self.config.control_port,
            clients_dir: self.config.clients_dir.clone(),
            sessions: Arc::new(dashboard::SessionStore::new(
                self.config.dashboard_auth.session_ttl_secs,
            )),
        };

        if self.config.dashboard_auth_enabled() {
            tracing::info!("🔒 Dashboard authentication enabled");
        } else {
            tracing::warn!("⚠️  Dashboard authentication disabled — dashboard is open to anyone");
        }

        tracing::info!("📦 Client install: https://dashboard.{}/install", domain);

        // ── Spawn control channel acceptor ──
        let reg_ctrl = registry.clone();
        let met_ctrl = metrics.clone();
        let config_ctrl = self.config.clone();
        let traefik_ctrl = traefik_manager.clone();
        let control_task = tokio::spawn(async move {
            Self::accept_clients(
                control_listener,
                reg_ctrl,
                met_ctrl,
                config_ctrl,
                traefik_ctrl,
            )
            .await;
        });

        // ── Public HTTP listener (subdomain proxy + dashboard) ──
        let public_listener = TcpListener::bind(("0.0.0.0", self.config.public_port))
            .await
            .with_context(|| format!("bind public port {}", self.config.public_port))?;
        tracing::info!("🌐 Public HTTP on :{}", self.config.public_port);

        let dashboard_router = dashboard::create_router(dash_state);
        tracing::info!("📊 Dashboard at dashboard.{}", domain);

        let reg_pub = registry.clone();
        let met_pub = metrics.clone();
        let rl_pub = rate_limiter.clone();
        let dom = domain.clone();
        let public_task = tokio::spawn(async move {
            Self::accept_public(
                public_listener,
                reg_pub,
                met_pub,
                rl_pub,
                dom,
                dashboard_router,
            )
            .await;
        });

        tracing::info!("✅ Zo Tunnel Server ready!");

        // ── Wait for shutdown ──
        let shutdown = async {
            tokio::signal::ctrl_c().await.ok();
            tracing::info!("🛑 Shutdown signal received, cleaning up...");
        };

        tokio::select! {
            _ = shutdown => {}
            _ = control_task => {
                tracing::error!("Control task ended unexpectedly");
            }
        }

        // ── Graceful cleanup ──
        tracing::info!("🧹 Cleaning up...");
        public_task.abort();

        if let Some(ref mgr) = traefik_manager {
            let _ = mgr.cleanup_clients();
        }

        let connected = registry.count();
        if connected > 0 {
            tracing::info!("🔌 Disconnecting {} client(s)...", connected);
        }

        let snap = metrics.snapshot();
        tracing::info!(
            "📊 Final stats: {} requests served, {} connections total, uptime {}s",
            snap.total_requests,
            snap.total_connections,
            snap.uptime_secs
        );
        tracing::info!("👋 Zo Tunnel Server stopped.");

        Ok(())
    }

    /// Accept and handle control channel connections from tunnel clients.
    async fn accept_clients(
        listener: TcpListener,
        registry: Arc<Registry>,
        metrics: Arc<Metrics>,
        config: ServerConfig,
        traefik: Option<Arc<crate::traefik::TraefikManager>>,
    ) {
        loop {
            match listener.accept().await {
                Ok((stream, addr)) => {
                    tracing::info!("📡 Client connecting from {}", addr);
                    metrics.total_connections.fetch_add(1, Ordering::Relaxed);

                    if let Err(e) = Self::configure_tcp_keepalive(&stream) {
                        tracing::warn!("⚠️  TCP keepalive failed for {}: {}", addr, e);
                    }

                    let reg = registry.clone();
                    let met = metrics.clone();
                    let cfg = config.clone();
                    let tf = traefik.clone();

                    tokio::spawn(async move {
                        if let Err(e) = Self::handle_client(stream, reg, met, cfg, tf).await {
                            tracing::warn!("Client {} error: {:#}", addr, e);
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("Control accept error: {}", e);
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            }
        }
    }

    /// Handle a single client session: auth → yamux → serve streams.
    async fn handle_client<S>(
        mut stream: S,
        registry: Arc<Registry>,
        metrics: Arc<Metrics>,
        config: ServerConfig,
        traefik: Option<Arc<crate::traefik::TraefikManager>>,
    ) -> Result<()>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let auth_msg = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            read_message(&mut stream),
        )
        .await
        .map_err(|_| anyhow::anyhow!("auth timeout: client did not send AuthReq within 10s"))?
        .context("read auth message")?;

        let (client_id, supports_heartbeat) = match auth_msg {
            Message::AuthReq(auth) => {
                tracing::info!("🔑 Auth from '{}'", auth.client_id);

                if !config.validate_token(&auth.token) {
                    let res = Message::AuthRes(AuthRes {
                        success: false,
                        message: "Invalid token".into(),
                        public_port: None,
                        assigned_route: None,
                    });
                    write_message(&mut stream, &res).await?;
                    metrics.failed_auth.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!("❌ Auth failed for '{}'", auth.client_id);
                    return Ok(());
                }

                let res = Message::AuthRes(AuthRes {
                    success: true,
                    message: format!("OK — {}.{}", auth.client_id, config.domain),
                    public_port: Some(config.public_port),
                    assigned_route: Some(auth.client_id.clone()),
                });
                write_message(&mut stream, &res).await?;

                tracing::info!(
                    "✅ '{}' authenticated → {}.{}",
                    auth.client_id,
                    auth.client_id,
                    config.domain
                );

                let supports_hb = auth.supports_heartbeat();
                (auth.client_id, supports_hb)
            }
            other => {
                tracing::warn!("Expected AuthReq, got {:?}", other);
                return Ok(());
            }
        };

        let last_activity = Arc::new(std::sync::Mutex::new(std::time::Instant::now()));
        let missed_heartbeats = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let (yamux_handle, driver_task) = spawn_yamux_driver(
            stream,
            yamux::Mode::Server,
            client_id.clone(),
            supports_heartbeat,
            last_activity.clone(),
            missed_heartbeats.clone(),
        );

        let entry = match registry.register(
            client_id.clone(),
            yamux_handle.clone(),
            supports_heartbeat,
            last_activity,
            missed_heartbeats,
        ) {
            Ok((e, replaced)) => {
                if replaced {
                    tracing::info!(
                        "♻️  Took over stale session for '{}' (total: {})",
                        client_id,
                        registry.count()
                    );
                } else {
                    tracing::info!(
                        "🟢 Client '{}' registered (total: {})",
                        client_id,
                        registry.count()
                    );
                }
                e
            }
            Err(e) => {
                tracing::warn!("Registration failed for '{}': {}", client_id, e);
                driver_task.abort();
                return Ok(());
            }
        };

        if let Some(ref mgr) = traefik {
            if let Err(e) = mgr.add_client(&client_id) {
                tracing::warn!("⚠️  Traefik add route failed: {:#}", e);
            }
        }

        let _ = driver_task.await;

        // Only drop the registry slot if this session is still current.
        // A takeover registers a new session_id; the old task must not
        // unregister the replacement. Keep the Traefik YAML so reconnects
        // do not wait for a file-provider reload.
        if registry.unregister(&client_id, entry.session_id) {
            tracing::info!(
                "🔴 Client '{}' disconnected (remaining: {})",
                client_id,
                registry.count()
            );
        } else {
            tracing::debug!(
                "♻️  Session for '{}' already replaced — skipping unregister",
                client_id
            );
        }

        Ok(())
    }

    /// Accept public HTTP connections — serves both tunnel proxy and dashboard.
    async fn accept_public(
        listener: TcpListener,
        registry: Arc<Registry>,
        metrics: Arc<Metrics>,
        rate_limiter: Arc<RateLimiter>,
        domain: String,
        dashboard_router: axum::Router,
    ) {
        loop {
            let (tcp_stream, addr) = match listener.accept().await {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!("Public accept error: {}", e);
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    continue;
                }
            };

            let reg = registry.clone();
            let met = metrics.clone();
            let rl = rate_limiter.clone();
            let dom = domain.clone();
            let dash = dashboard_router.clone();

            tokio::spawn(async move {
                let result =
                    Self::serve_http(TokioIo::new(tcp_stream), reg, met, rl, dom, dash).await;
                if let Err(e) = result {
                    tracing::debug!("HTTP error from {}: {}", addr, e);
                }
            });
        }
    }

    /// Serve a single HTTP connection — routes to dashboard or tunnel proxy by subdomain.
    async fn serve_http<I>(
        io: I,
        registry: Arc<Registry>,
        metrics: Arc<Metrics>,
        rate_limiter: Arc<RateLimiter>,
        domain: String,
        dashboard_router: axum::Router,
    ) -> Result<()>
    where
        I: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
    {
        let dashboard_host = format!("dashboard.{}", domain);

        http1::Builder::new()
            .serve_connection(
                io,
                service_fn(move |req: Request<hyper::body::Incoming>| {
                    let reg = registry.clone();
                    let met = metrics.clone();
                    let rl = rate_limiter.clone();
                    let dom = domain.clone();
                    let dash = dashboard_router.clone();
                    let dash_host = dashboard_host.clone();
                    async move {
                        // Check Host header to decide routing
                        let host = req
                            .headers()
                            .get("host")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("");
                        let host_no_port = host.split(':').next().unwrap_or(host);

                        // Install/download on apex, dashboard on dashboard.<domain>
                        if host_no_port == dash_host || host_no_port == dom {
                            Self::handle_dashboard(req, dash).await
                        } else {
                            proxy::handle_proxy_request(req, reg, met, rl, dom).await
                        }
                    }
                }),
            )
            .await
            .context("serve HTTP connection")?;
        Ok(())
    }

    /// Forward a request to the dashboard axum app.
    async fn handle_dashboard(
        req: Request<hyper::body::Incoming>,
        router: axum::Router,
    ) -> Result<Response<proxy::BoxBody>, hyper::Error> {
        use tower::ServiceExt;

        // Convert hyper Incoming body → axum Body
        let (parts, body) = req.into_parts();
        let axum_body = axum::body::Body::new(body);
        let axum_req = Request::from_parts(parts, axum_body);

        // Call the axum router
        let axum_resp = router.oneshot(axum_req).await.unwrap_or_else(|_| {
            Response::builder()
                .status(500)
                .body(axum::body::Body::empty())
                .unwrap()
        });

        // Convert axum response → proxy BoxBody by collecting bytes
        let (parts, axum_body) = axum_resp.into_parts();
        let collected = axum_body
            .collect()
            .await
            .unwrap_or_else(|_| http_body_util::Collected::default());
        let box_body = http_body_util::Full::new(collected.to_bytes())
            .map_err(|never| match never {})
            .boxed();

        Ok(Response::from_parts(parts, box_body))
    }

    /// Configure TCP keepalive on a client socket.
    fn configure_tcp_keepalive(stream: &tokio::net::TcpStream) -> Result<()> {
        use socket2::SockRef;

        let sock = SockRef::from(stream);
        let keepalive = socket2::TcpKeepalive::new()
            .with_time(Duration::from_secs(HEARTBEAT_INTERVAL_SECS))
            .with_interval(Duration::from_secs(5));

        #[cfg(target_os = "linux")]
        let keepalive = keepalive.with_retries(3);

        sock.set_tcp_keepalive(&keepalive)
            .context("set TCP keepalive")?;
        Ok(())
    }
}
