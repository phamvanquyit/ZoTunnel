//! Traefik file-provider integration (host Traefik + shared dynamic dir).
//!
//! Pattern (matches existing Zobite Traefik setup):
//! - `_zo-tunnel-base.yml` — shared service + dashboard/apex routers
//! - `zo-tunnel-{client}.yml` — router only, points at shared service

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const SHARED_SERVICE: &str = "zo-tunnel-server";
const BASE_FILE: &str = "_zo-tunnel-base.yml";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraefikConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_config_dir")]
    pub config_dir: String,
    #[serde(default = "default_entrypoint")]
    pub entrypoint: String,
    #[serde(default = "default_cert_resolver")]
    pub cert_resolver: String,
    #[serde(default = "default_service_url")]
    pub service_url: String,
}

impl Default for TraefikConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            config_dir: default_config_dir(),
            entrypoint: default_entrypoint(),
            cert_resolver: default_cert_resolver(),
            service_url: default_service_url(),
        }
    }
}

fn default_config_dir() -> String {
    "/etc/traefik/dynamic".into()
}
fn default_entrypoint() -> String {
    "websecure".into()
}
fn default_cert_resolver() -> String {
    "letsencrypt".into()
}
fn default_service_url() -> String {
    "http://127.0.0.1:6210".into()
}

impl TraefikConfig {
    pub fn auto_detect() -> Self {
        for dir in ["/etc/traefik/dynamic", "/etc/traefik/conf.d"] {
            if Path::new(dir).is_dir() {
                return Self {
                    enabled: true,
                    config_dir: dir.into(),
                    ..Default::default()
                };
            }
        }
        Self::default()
    }
}

pub struct TraefikManager {
    config: TraefikConfig,
    domain: String,
}

impl TraefikManager {
    pub fn new(config: TraefikConfig, domain: String) -> Self {
        Self { config, domain }
    }

    pub fn ensure_dir(&self) -> anyhow::Result<()> {
        std::fs::create_dir_all(&self.config.config_dir)?;
        Ok(())
    }

    /// Write shared service + separate routers for dashboard and apex.
    pub fn write_static_routes(&self) -> anyhow::Result<()> {
        self.ensure_dir()?;
        let content = format!(
            r#"http:
  routers:
    zo-tunnel-dashboard:
      rule: "Host(`dashboard.{domain}`)"
      entryPoints:
        - "{ep}"
      service: {svc}
      tls:
        certResolver: {cr}
    zo-tunnel-apex:
      rule: "Host(`{domain}`)"
      entryPoints:
        - "{ep}"
      service: {svc}
      tls:
        certResolver: {cr}
  services:
    {svc}:
      loadBalancer:
        servers:
          - url: "{url}"
"#,
            domain = self.domain,
            ep = self.config.entrypoint,
            svc = SHARED_SERVICE,
            cr = self.config.cert_resolver,
            url = self.config.service_url,
        );
        let path = Path::new(&self.config.config_dir).join(BASE_FILE);
        std::fs::write(&path, content)?;
        tracing::info!("🔀 Traefik: wrote {}", path.display());
        Ok(())
    }

    pub fn add_client(&self, client_id: &str) -> anyhow::Result<()> {
        let safe = sanitize_id(client_id);
        let host = format!("{}.{}", client_id, self.domain);
        let content = format!(
            r#"http:
  routers:
    zo-tunnel-{safe}:
      rule: "Host(`{host}`)"
      entryPoints:
        - "{ep}"
      service: {svc}
      tls:
        certResolver: {cr}
"#,
            safe = safe,
            host = host,
            ep = self.config.entrypoint,
            svc = SHARED_SERVICE,
            cr = self.config.cert_resolver,
        );
        let path = self.client_file(&safe);
        if path.exists() {
            if let Ok(existing) = std::fs::read_to_string(&path) {
                if existing == content {
                    return Ok(());
                }
            }
        }
        std::fs::write(&path, content)?;
        tracing::info!("🔀 Traefik: added route for {}", host);
        Ok(())
    }

    /// Delete a client's Traefik route file.
    /// Not used on disconnect/shutdown — keeping the router avoids a Traefik
    /// reload gap (and public 404) while the client reconnects.
    #[allow(dead_code)]
    pub fn remove_client(&self, client_id: &str) -> anyhow::Result<()> {
        let safe = sanitize_id(client_id);
        let path = self.client_file(&safe);
        if path.exists() {
            std::fs::remove_file(&path)?;
            tracing::info!("🔀 Traefik: removed {}", path.display());
        }
        Ok(())
    }

    /// Remove legacy `zo-*.yml` leftovers only. Client routers
    /// (`zo-tunnel-{name}.yml`) are intentionally kept so a server restart
    /// does not blank public hosts before clients reconnect.
    #[allow(dead_code)]
    pub fn cleanup_legacy_routes(&self) -> anyhow::Result<()> {
        let dir = Path::new(&self.config.config_dir);
        if !dir.is_dir() {
            return Ok(());
        }
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            // legacy zo-*.yml (except base / current zo-tunnel-* routers)
            if name.starts_with("zo-")
                && name.ends_with(".yml")
                && !name.starts_with("zo-tunnel-")
                && name != "zo-static.yml"
            {
                let _ = std::fs::remove_file(entry.path());
            }
        }
        Ok(())
    }

    /// Old name kept for callers/tests; does not delete live client routers.
    #[allow(dead_code)]
    pub fn cleanup_clients(&self) -> anyhow::Result<()> {
        self.cleanup_legacy_routes()
    }

    fn client_file(&self, safe_id: &str) -> PathBuf {
        Path::new(&self.config.config_dir).join(format!("zo-tunnel-{}.yml", safe_id))
    }
}

fn sanitize_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_keeps_safe_chars() {
        assert_eq!(sanitize_id("my-api_1"), "my-api_1");
        assert_eq!(sanitize_id("bad/name"), "bad_name");
    }

    #[test]
    fn add_client_skips_identical_rewrite() {
        let dir = std::env::temp_dir().join(format!("zo-tunnel-traefik-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let mgr = TraefikManager::new(
            TraefikConfig {
                enabled: true,
                config_dir: dir.to_string_lossy().into(),
                ..Default::default()
            },
            "tunnel.example.com".into(),
        );
        mgr.add_client("omnivoice").unwrap();
        let path = dir.join("zo-tunnel-omnivoice.yml");
        let first = std::fs::read_to_string(&path).unwrap();
        let mtime1 = std::fs::metadata(&path).unwrap().modified().unwrap();
        mgr.add_client("omnivoice").unwrap();
        let second = std::fs::read_to_string(&path).unwrap();
        let mtime2 = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(first, second);
        assert_eq!(mtime1, mtime2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cleanup_clients_keeps_live_routers() {
        let dir = std::env::temp_dir().join(format!("zo-tunnel-traefik-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let mgr = TraefikManager::new(
            TraefikConfig {
                enabled: true,
                config_dir: dir.to_string_lossy().into(),
                ..Default::default()
            },
            "tunnel.example.com".into(),
        );
        mgr.add_client("omnivoice").unwrap();
        let legacy = dir.join("zo-oldstyle.yml");
        std::fs::write(&legacy, "legacy\n").unwrap();
        mgr.cleanup_clients().unwrap();
        assert!(dir.join("zo-tunnel-omnivoice.yml").exists());
        assert!(!legacy.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
