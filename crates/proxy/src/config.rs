//! Configuration layering: environment > `config.toml` > built-in defaults.
//!
//! Only values a human would want to tune live here. Protocol constants (API
//! paths, header names, the PoW algorithm) stay in code — putting them in a
//! config file invites silent divergence from the server.

use serde::Deserialize;
use std::path::Path;

/// File-backed configuration. Every field is optional; unset means "use the
/// default", so a partial `config.toml` is valid.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct FileConfig {
    pub proxy: ProxySection,
    pub storage: StorageSection,
    pub deepseek: DeepseekSection,
    pub limits: LimitsSection,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct ProxySection {
    pub host: String,
    pub port: u16,
}

impl Default for ProxySection {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".into(),
            port: 9174,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct StorageSection {
    pub db_path: String,
}

impl Default for StorageSection {
    fn default() -> Self {
        Self {
            db_path: "deepseek-rs.db".into(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct DeepseekSection {
    /// Model id exposed when the client does not send one.
    pub model: String,
    /// Default thinking flag when the request does not specify one.
    pub thinking: bool,
    /// Send the web-search flag. Off by default: thinking+search is throttled
    /// hard by the backend and turns transient limits into hard failures.
    pub search: bool,
}

impl Default for DeepseekSection {
    fn default() -> Self {
        Self {
            model: "deepseek-chat".into(),
            thinking: false,
            search: false,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct LimitsSection {
    /// Seconds to wait after one request finishes before starting the next on
    /// the same account (completion-to-start, not start-to-start).
    pub request_delay_secs: f64,
    /// Requests allowed to queue behind a busy account before rejecting.
    pub queue_limit: usize,
    /// Cooldown for an account that fails to log in, in seconds.
    pub login_cooldown_secs: u64,
}

impl Default for LimitsSection {
    fn default() -> Self {
        Self {
            request_delay_secs: 2.0,
            queue_limit: 64,
            login_cooldown_secs: 300,
        }
    }
}

/// Fully resolved configuration.
#[derive(Debug, Clone)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub db_path: String,
    pub model: String,
    pub thinking: bool,
    pub search: bool,
    pub request_delay_secs: f64,
    pub queue_limit: usize,
    pub login_cooldown_secs: u64,
}

impl Config {
    /// Resolve config: `config.toml` (if present) then environment overrides.
    ///
    /// `CONFIG_PATH` selects a different file; a missing file is not an error.
    pub fn load() -> Result<Self, String> {
        let path = std::env::var("CONFIG_PATH").unwrap_or_else(|_| "config.toml".into());
        let file = Self::read_file(&path)?;

        Ok(Self {
            host: env_str("PROXY_HOST").unwrap_or(file.proxy.host),
            port: env_num("PROXY_PORT").unwrap_or(file.proxy.port),
            db_path: env_str("DB_PATH").unwrap_or(file.storage.db_path),
            model: env_str("MODEL_NAME").unwrap_or(file.deepseek.model),
            thinking: env_flag("THINKING_ENABLED", file.deepseek.thinking),
            search: env_flag("SEARCH_ENABLED", file.deepseek.search),
            request_delay_secs: env_num("REQUEST_DELAY")
                .unwrap_or(file.limits.request_delay_secs)
                .max(0.0),
            queue_limit: env_num("QUEUE_LIMIT").unwrap_or(file.limits.queue_limit),
            login_cooldown_secs: env_num("LOGIN_COOLDOWN")
                .unwrap_or(file.limits.login_cooldown_secs),
        })
    }

    fn read_file(path: &str) -> Result<FileConfig, String> {
        if !Path::new(path).exists() {
            return Ok(FileConfig::default());
        }
        let raw = std::fs::read_to_string(path).map_err(|e| format!("read {path}: {e}"))?;
        toml::from_str(&raw).map_err(|e| format!("parse {path}: {e}"))
    }
}

fn env_str(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

fn env_num<T: std::str::FromStr>(key: &str) -> Option<T> {
    env_str(key).and_then(|v| v.trim().parse().ok())
}

fn env_flag(key: &str, default: bool) -> bool {
    match env_str(key) {
        Some(v) => matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        None => default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_toml_keeps_defaults() {
        let f: FileConfig = toml::from_str("[proxy]\nport = 9999\n").unwrap();
        assert_eq!(f.proxy.port, 9999);
        assert_eq!(f.proxy.host, "0.0.0.0");
        assert_eq!(f.limits.request_delay_secs, 2.0);
        assert!(!f.deepseek.search);
    }

    #[test]
    fn empty_toml_is_all_defaults() {
        let f: FileConfig = toml::from_str("").unwrap();
        assert_eq!(f.proxy.port, 9174);
        assert_eq!(f.storage.db_path, "deepseek-rs.db");
        assert_eq!(f.limits.queue_limit, 64);
    }

    #[test]
    fn missing_file_is_not_an_error() {
        let f = Config::read_file("/nonexistent/config.toml").unwrap();
        assert_eq!(f.proxy.port, 9174);
    }
}
