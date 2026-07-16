//! zotunnel client config — `~/.zotunnel/config.yaml`

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClientTlsConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub server_name: String,
    #[serde(default)]
    pub skip_verify: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZotunnelConfig {
    pub server: String,
    pub token: String,
    #[serde(default)]
    pub tls: ClientTlsConfig,
}

pub fn config_dir() -> anyhow::Result<PathBuf> {
    let home =
        std::env::var("HOME").map_err(|_| anyhow::anyhow!("HOME environment variable not set"))?;
    Ok(PathBuf::from(home).join(".zotunnel"))
}

pub fn config_path() -> anyhow::Result<PathBuf> {
    Ok(config_dir()?.join("config.yaml"))
}

impl ZotunnelConfig {
    pub fn load() -> anyhow::Result<Self> {
        let path = config_path()?;
        if !path.exists() {
            anyhow::bail!(
                "No config found at {}. Run: zotunnel config set --server HOST:6200 --token TOKEN",
                path.display()
            );
        }
        let content = std::fs::read_to_string(&path)?;
        Ok(serde_yaml::from_str(&content)?)
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let dir = config_dir()?;
        std::fs::create_dir_all(&dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        }
        let path = config_path()?;
        std::fs::write(&path, serde_yaml::to_string(self)?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }

    pub fn masked_token(&self) -> String {
        if self.token.len() <= 4 {
            "****".into()
        } else {
            format!("{}****", &self.token[..4])
        }
    }
}

pub fn generate_name() -> String {
    let mut buf = [0u8; 4];
    getrandom::getrandom(&mut buf).expect("getrandom");
    format!(
        "tunnel-{}",
        buf.iter().map(|b| format!("{:02x}", b)).collect::<String>()
    )
}

pub fn normalize_local_addr(addr: &str) -> String {
    if addr.contains(':') {
        addr.to_string()
    } else {
        format!("localhost:{}", addr)
    }
}
