//! Dashboard REST API + static file server with authentication.
//! Also serves /install and /download for client one-line install (apex host).

use crate::metrics::Metrics;
use crate::registry::Registry;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::Router;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Instant;

struct Session {
    created_at: Instant,
}

pub struct SessionStore {
    sessions: DashMap<String, Session>,
    ttl_secs: u64,
}

impl SessionStore {
    pub fn new(ttl_secs: u64) -> Self {
        Self {
            sessions: DashMap::new(),
            ttl_secs,
        }
    }

    pub fn create(&self) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        self.sessions.insert(
            id.clone(),
            Session {
                created_at: Instant::now(),
            },
        );
        id
    }

    pub fn validate(&self, session_id: &str) -> bool {
        if let Some(entry) = self.sessions.get(session_id) {
            if entry.created_at.elapsed().as_secs() < self.ttl_secs {
                return true;
            }
            drop(entry);
            self.sessions.remove(session_id);
        }
        false
    }

    pub fn invalidate(&self, session_id: &str) {
        self.sessions.remove(session_id);
    }
}

#[derive(Clone)]
pub struct DashboardState {
    pub registry: Arc<Registry>,
    pub metrics: Arc<Metrics>,
    pub dashboard_token: String,
    pub client_token: String,
    pub auth_enabled: bool,
    pub tls_enabled: bool,
    pub domain: String,
    pub control_port: u16,
    pub clients_dir: String,
    pub sessions: Arc<SessionStore>,
}

pub fn create_router(state: DashboardState) -> Router {
    Router::new()
        .route("/", get(dashboard_ui))
        .route("/style.css", get(dashboard_css))
        .route("/app.js", get(dashboard_js))
        .route("/api/login", post(api_login))
        .route("/api/auth/check", get(api_auth_check))
        .route("/api/status", get(api_status))
        .route("/api/clients", get(api_clients))
        .route("/api/metrics", get(api_metrics))
        .route("/api/logout", post(api_logout))
        .route("/install", get(serve_install_script))
        .route("/download/:filename", get(serve_download))
        .with_state(state)
}

const COOKIE_NAME: &str = "zo-session";

fn extract_session_id(headers: &HeaderMap) -> Option<String> {
    let cookie_header = headers.get(header::COOKIE)?.to_str().ok()?;
    for part in cookie_header.split(';') {
        let part = part.trim();
        if let Some(value) = part.strip_prefix(COOKIE_NAME) {
            let value = value.strip_prefix('=')?;
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

fn build_session_cookie(session_id: &str, max_age_secs: u64, tls_enabled: bool) -> String {
    let mut cookie = format!(
        "{}={}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}",
        COOKIE_NAME, session_id, max_age_secs
    );
    if tls_enabled {
        cookie.push_str("; Secure");
    }
    cookie
}

fn build_clear_cookie() -> String {
    format!(
        "{}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0",
        COOKIE_NAME
    )
}

fn is_authenticated(state: &DashboardState, headers: &HeaderMap) -> bool {
    if !state.auth_enabled {
        return true;
    }
    if let Some(session_id) = extract_session_id(headers) {
        return state.sessions.validate(&session_id);
    }
    false
}

#[derive(Deserialize)]
struct LoginRequest {
    token: String,
}

#[derive(Serialize)]
struct LoginResponse {
    success: bool,
    message: String,
}

#[derive(Serialize)]
struct AuthCheckResponse {
    authenticated: bool,
    auth_required: bool,
    tls_enabled: bool,
}

fn is_tls_enabled(state: &DashboardState, headers: &HeaderMap) -> bool {
    if state.tls_enabled {
        return true;
    }
    if let Some(proto) = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
    {
        if proto.trim().eq_ignore_ascii_case("https") {
            return true;
        }
    }
    false
}

async fn api_login(
    State(state): State<DashboardState>,
    headers: HeaderMap,
    Json(payload): Json<LoginRequest>,
) -> impl IntoResponse {
    if !state.auth_enabled {
        return (
            StatusCode::OK,
            HeaderMap::new(),
            Json(LoginResponse {
                success: true,
                message: "Authentication not required".into(),
            }),
        );
    }

    if is_authenticated(&state, &headers) {
        return (
            StatusCode::OK,
            HeaderMap::new(),
            Json(LoginResponse {
                success: true,
                message: "Already authenticated".into(),
            }),
        );
    }

    use crate::config::ServerConfig;
    let mut check_cfg = ServerConfig::default();
    check_cfg.dashboard_auth.token = state.dashboard_token.clone();

    if check_cfg.validate_dashboard_token(&payload.token) {
        let session_id = state.sessions.create();
        let tls_enabled = is_tls_enabled(&state, &headers);
        let cookie = build_session_cookie(&session_id, state.sessions.ttl_secs, tls_enabled);

        let mut resp_headers = HeaderMap::new();
        resp_headers.insert(
            header::SET_COOKIE,
            cookie.parse().expect("valid cookie header"),
        );

        tracing::info!("🔓 Dashboard login successful");

        (
            StatusCode::OK,
            resp_headers,
            Json(LoginResponse {
                success: true,
                message: "Login successful".into(),
            }),
        )
    } else {
        tracing::warn!("🔒 Dashboard login failed: invalid token");

        (
            StatusCode::UNAUTHORIZED,
            HeaderMap::new(),
            Json(LoginResponse {
                success: false,
                message: "Invalid admin token".into(),
            }),
        )
    }
}

async fn api_auth_check(
    State(state): State<DashboardState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    Json(AuthCheckResponse {
        authenticated: is_authenticated(&state, &headers),
        auth_required: state.auth_enabled,
        tls_enabled: is_tls_enabled(&state, &headers),
    })
}

async fn api_logout(State(state): State<DashboardState>, headers: HeaderMap) -> impl IntoResponse {
    if let Some(session_id) = extract_session_id(&headers) {
        state.sessions.invalidate(&session_id);
    }

    let mut resp_headers = HeaderMap::new();
    resp_headers.insert(
        header::SET_COOKIE,
        build_clear_cookie().parse().expect("valid cookie header"),
    );

    (
        StatusCode::OK,
        resp_headers,
        Json(LoginResponse {
            success: true,
            message: "Logged out".into(),
        }),
    )
}

#[derive(Serialize)]
struct StatusResponse {
    status: &'static str,
    version: &'static str,
    connected_clients: usize,
    domain: String,
    control_port: u16,
    client_token: String,
    install_url: String,
    install_command: String,
    config_command: String,
    example_command: String,
}

async fn api_status(State(state): State<DashboardState>, headers: HeaderMap) -> impl IntoResponse {
    if !is_authenticated(&state, &headers) {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "Authentication required"})),
        ));
    }
    let install_url = format!("https://dashboard.{}/install", state.domain);
    let install_command = format!("curl -sSL {} | bash", install_url);
    let config_command = format!(
        "zotunnel config set --server {}:{} --token {}",
        state.domain, state.control_port, state.client_token
    );
    let example_command = "zotunnel http 3000 --name my-app -d".to_string();
    Ok(Json(StatusResponse {
        status: "running",
        version: env!("CARGO_PKG_VERSION"),
        connected_clients: state.registry.count(),
        domain: state.domain.clone(),
        control_port: state.control_port,
        client_token: state.client_token.clone(),
        install_url,
        install_command,
        config_command,
        example_command,
    }))
}

async fn api_clients(State(state): State<DashboardState>, headers: HeaderMap) -> impl IntoResponse {
    if !is_authenticated(&state, &headers) {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "Authentication required"})),
        ));
    }
    Ok(Json(state.registry.list()))
}

async fn api_metrics(State(state): State<DashboardState>, headers: HeaderMap) -> impl IntoResponse {
    if !is_authenticated(&state, &headers) {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "Authentication required"})),
        ));
    }
    Ok(Json(state.metrics.snapshot()))
}

async fn serve_install_script(State(state): State<DashboardState>) -> impl IntoResponse {
    let script = format!(
        r#"#!/usr/bin/env bash
set -euo pipefail

DOMAIN="{domain}"
CONTROL_PORT="{control_port}"
BASE="https://dashboard.${{DOMAIN}}"
INSTALL_DIR="$HOME/.zotunnel/bin"
mkdir -p "$INSTALL_DIR"

OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
ARCH="$(uname -m)"
case "$OS" in
  linux) OS_LABEL=linux ;;
  darwin) OS_LABEL=darwin ;;
  *) echo "Unsupported OS: $OS"; exit 1 ;;
esac
case "$ARCH" in
  x86_64|amd64) ARCH_LABEL=amd64 ;;
  aarch64|arm64) ARCH_LABEL=arm64 ;;
  *) echo "Unsupported arch: $ARCH"; exit 1 ;;
esac

FILE="zotunnel-${{OS_LABEL}}-${{ARCH_LABEL}}"
URL="${{BASE}}/download/${{FILE}}"
SRC_URL="${{BASE}}/download/zotunnel-src.tar.gz"

install_from_source() {{
  echo "▸ Prebuilt binary not available for ${{OS_LABEL}}-${{ARCH_LABEL}}"
  echo "▸ Building zotunnel from source (needs Rust + network)..."
  if ! command -v cargo >/dev/null 2>&1; then
    echo "▸ Installing Rust toolchain..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
    # shellcheck disable=SC1091
    source "$HOME/.cargo/env"
  fi
  TMP="$(mktemp -d)"
  trap 'rm -rf "$TMP"' EXIT
  curl -fsSL "$SRC_URL" -o "$TMP/src.tar.gz"
  mkdir -p "$TMP/src"
  tar -xzf "$TMP/src.tar.gz" -C "$TMP/src"
  cargo build --release --manifest-path "$TMP/src/Cargo.toml" -p zo-tunnel-client
  cp "$TMP/src/target/release/zotunnel" "$INSTALL_DIR/zotunnel"
  chmod +x "$INSTALL_DIR/zotunnel"
}}

echo "▸ Downloading ${{FILE}}..."
if curl -fsSL "$URL" -o "$INSTALL_DIR/zotunnel"; then
  chmod +x "$INSTALL_DIR/zotunnel"
else
  install_from_source
fi

SHELL_RC=""
case "${{SHELL:-}}" in
  */zsh) SHELL_RC="$HOME/.zshrc" ;;
  */bash) SHELL_RC="$HOME/.bashrc" ;;
  *) SHELL_RC="$HOME/.profile" ;;
esac
if [ -n "$SHELL_RC" ] && ! grep -q '.zotunnel/bin' "$SHELL_RC" 2>/dev/null; then
  echo 'export PATH="$HOME/.zotunnel/bin:$PATH"' >> "$SHELL_RC"
fi
export PATH="$INSTALL_DIR:$PATH"

echo "✅ zotunnel installed to $INSTALL_DIR/zotunnel"
echo
echo "Next:"
echo "  zotunnel config set --server ${{DOMAIN}}:${{CONTROL_PORT}} --token <TOKEN>"
echo "  zotunnel http 3000"
"#,
        domain = state.domain,
        control_port = state.control_port,
    );

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        script,
    )
}

async fn serve_download(
    State(state): State<DashboardState>,
    axum::extract::Path(filename): axum::extract::Path<String>,
) -> Result<Response, (StatusCode, String)> {
    if !filename
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        || filename.contains("..")
    {
        return Err((StatusCode::BAD_REQUEST, "invalid filename".to_string()));
    }

    let path = std::path::Path::new(&state.clients_dir).join(&filename);
    let (path, disposition) = if path.is_file() {
        (path, filename.clone())
    } else {
        let alt = std::path::Path::new(&state.clients_dir).join(format!("{}.tar.gz", filename));
        if alt.is_file() {
            let name = alt.file_name().unwrap().to_string_lossy().into_owned();
            (alt, name)
        } else {
            return Err((StatusCode::NOT_FOUND, format!("{} not found", filename)));
        }
    };

    let data = std::fs::read(&path).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("read error: {}", e),
        )
    })?;

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{}\"", disposition),
        )
        .body(axum::body::Body::from(data))
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

async fn dashboard_ui() -> impl IntoResponse {
    Html(include_str!("../../../web/index.html"))
}

async fn dashboard_css() -> impl IntoResponse {
    (
        StatusCode::OK,
        [("content-type", "text/css")],
        include_str!("../../../web/style.css"),
    )
}

async fn dashboard_js() -> impl IntoResponse {
    (
        StatusCode::OK,
        [("content-type", "application/javascript")],
        include_str!("../../../web/app.js"),
    )
}
