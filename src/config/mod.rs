//! Config Manager
//!
//! Loads/saves `config.toml`. Also handles importing `~/.ssh/config`
//! (OpenSSH config format) into HyperTerm session profiles.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub mod ssh_config_import;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub general: GeneralConfig,
    #[serde(default)]
    pub terminal: TerminalConfig,
    #[serde(default)]
    pub scrollback: ScrollbackConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
    #[serde(default)]
    pub sessions: Vec<SessionProfile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeneralConfig {
    pub theme: Theme,
    pub font_family: String,
    pub font_size: u16,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            theme: Theme::Dark,
            font_family: "Cascadia Code".into(),
            font_size: 14,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Theme {
    Dark,
    Light,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalConfig {
    /// Reconnect automatically on unexpected disconnect.
    pub auto_reconnect: bool,
    pub reconnect_backoff_ms: u64,
    pub max_reconnect_attempts: u32,
    /// SSH keepalive interval.
    pub keepalive_interval_secs: u64,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            auto_reconnect: true,
            reconnect_backoff_ms: 1000,
            max_reconnect_attempts: 20,
            keepalive_interval_secs: 15,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScrollbackConfig {
    /// Lines kept fully in RAM before spilling to disk cache.
    pub ram_line_capacity: usize,
    /// Directory (relative or absolute) holding per-session disk caches.
    pub cache_dir: PathBuf,
    /// Keep history across restarts.
    pub persistent: bool,
}

impl Default for ScrollbackConfig {
    fn default() -> Self {
        Self {
            ram_line_capacity: 100_000,
            cache_dir: PathBuf::from("logs"),
            persistent: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    pub level: String,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: "info".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionProfile {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: AuthMethod,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum AuthMethod {
    Password,
    PublicKey {
        private_key_path: PathBuf,
        passphrase_env_var: Option<String>,
    },
    Agent,
}

pub fn config_path() -> PathBuf {
    if let Some(proj) = directories::ProjectDirs::from("dev", "HyperTerm", "HyperTerm") {
        proj.config_dir().join("config.toml")
    } else {
        PathBuf::from("config.toml")
    }
}

pub fn load_or_default() -> Result<AppConfig> {
    let path = config_path();
    if path.exists() {
        let raw =
            fs::read_to_string(&path).with_context(|| format!("reading config at {path:?}"))?;
        let cfg: AppConfig =
            toml::from_str(&raw).with_context(|| format!("parsing config at {path:?}"))?;
        tracing::info!(target: "hyperterm::config", "loaded config from {:?}", path);
        Ok(cfg)
    } else {
        let cfg = AppConfig::default();
        save(&cfg)?;
        tracing::info!(target: "hyperterm::config", "no config found, wrote default to {:?}", path);
        Ok(cfg)
    }
}

pub fn save(cfg: &AppConfig) -> Result<()> {
    let path = config_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let raw = toml::to_string_pretty(cfg)?;
    fs::write(&path, raw)?;
    Ok(())
}

/// Returns a copy of the config serialized to TOML with secrets stripped,
/// safe to embed in a diagnostic report.
pub fn redacted_toml(cfg: &AppConfig) -> String {
    let mut clone = cfg.clone();
    for s in &mut clone.sessions {
        if let AuthMethod::PublicKey {
            private_key_path, ..
        } = &mut s.auth
        {
            *private_key_path = PathBuf::from("<redacted>");
        }
    }
    toml::to_string_pretty(&clone).unwrap_or_else(|_| "<failed to serialize config>".into())
}

pub fn default_ssh_config_path() -> PathBuf {
    if let Some(home) = directories::BaseDirs::new() {
        home.home_dir().join(".ssh").join("config")
    } else {
        Path::new(".ssh/config").to_path_buf()
    }
}
