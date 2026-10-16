use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::Deserialize;

/// Server settings are loaded exclusively from JSON. Relative paths belong to
/// the configuration file, so the same file works from any working directory.
#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub admin_listen: SocketAddr,
    pub tunnel_listen: SocketAddr,
    pub public_url: String,
    pub data_dir: PathBuf,
    pub admin_password: Option<String>,
    pub reset_password: bool,
    pub tls_cert: Option<PathBuf>,
    pub tls_key: Option<PathBuf>,
    pub allow_insecure: bool,
    pub port_min: u16,
    pub port_max: u16,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            admin_listen: SocketAddr::from(([127, 0, 0, 1], 7400)),
            tunnel_listen: SocketAddr::from(([0, 0, 0, 0], 7443)),
            public_url: "wss://localhost:7443".into(),
            data_dir: "data/server".into(),
            admin_password: None,
            reset_password: false,
            tls_cert: None,
            tls_key: None,
            allow_insecure: false,
            port_min: 10000,
            port_max: 60000,
        }
    }
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let contents = std::fs::read_to_string(path).with_context(|| {
            format!(
                "无法读取服务端配置 {}（默认读取当前目录的 server.json）",
                path.display()
            )
        })?;
        let mut config: Self = serde_json::from_str(contents.trim_start_matches('\u{feff}'))
            .with_context(|| format!("服务端 JSON 配置格式错误: {}", path.display()))?;
        let absolute = std::fs::canonicalize(path)
            .with_context(|| format!("无法定位服务端配置 {}", path.display()))?;
        let base = absolute.parent().context("配置文件没有有效的父目录")?;
        if config.data_dir.is_relative() {
            config.data_dir = base.join(&config.data_dir);
        }
        for path in [&mut config.tls_cert, &mut config.tls_key]
            .into_iter()
            .flatten()
        {
            if path.is_relative() {
                *path = base.join(&*path);
            }
        }
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_belong_to_config_directory_and_omitted_fields_keep_safe_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server.json");
        std::fs::write(
            &path,
            r#"{"tls_cert":"certs/fullchain.pem","tls_key":"certs/key.pem"}"#,
        )
        .unwrap();
        let config = Config::load(&path).unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        assert_eq!(config.data_dir, base.join("data/server"));
        assert_eq!(config.tls_cert.unwrap(), base.join("certs/fullchain.pem"));
        assert_eq!(config.tls_key.unwrap(), base.join("certs/key.pem"));
        assert_eq!(config.admin_listen, "127.0.0.1:7400".parse().unwrap());
        assert!(!config.allow_insecure);
        assert!(config.admin_password.is_none());
    }

    #[test]
    fn configuration_typos_are_errors_instead_of_silent_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server.json");
        std::fs::write(&path, r#"{"tunnel_lisen":"127.0.0.1:7443"}"#).unwrap();
        let error = Config::load(path).err().unwrap();
        assert!(format!("{error:#}").contains("unknown field"));
    }

    #[test]
    fn utf8_bom_and_absolute_paths_are_supported() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server.json");
        let data = dir.path().join("state");
        let contents = serde_json::json!({"data_dir": data});
        std::fs::write(&path, format!("\u{feff}{contents}")).unwrap();
        assert_eq!(Config::load(path).unwrap().data_dir, data);
    }
}
