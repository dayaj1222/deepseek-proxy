//! Configuration layering: environment > `config.toml` > built-in defaults.
//!
//! Only values a human would want to tune live here. Protocol constants (API
//! paths, header names, the PoW algorithm) stay in code — putting them in a
//! config file invites silent divergence from the server.

use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

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
    /// Wire dialect: `json_invoke` (default), `deepseek_v4_dsml`, or `simple_tool_tag`.
    pub dialect: Option<String>,
}

impl Default for ProxySection {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".into(),
            port: 9174,
            dialect: None,
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
    /// Email addresses to leave out of the pool, e.g.
    /// `disabled_accounts = ["a@b.com", "c@d.com"]`.
    ///
    /// Email is the key because it is the one identifier every account has,
    /// whether it came from `.env` (`DEEPSEEK_EMAIL_<i>`) or was added with
    /// `deepseek-rs accounts add`. Matching is case-insensitive. A disabled
    /// account is never turned into a spec: no login is attempted, it never
    /// enters the pool, and no thread is ever bound to it. Its credentials
    /// stay where they are, so re-enabling is a one-line config change.
    pub disabled_accounts: Vec<DisabledAccount>,
}

impl Default for DeepseekSection {
    fn default() -> Self {
        Self {
            model: "deepseek-chat".into(),
            thinking: false,
            search: false,
            disabled_accounts: Vec::new(),
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
    /// Max time to wait for a busy account's slot, in seconds. Python:
    /// `QUEUE_TIMEOUT` (default 120).
    pub queue_timeout_secs: f64,
    /// Cooldown for an account that fails to log in, in seconds.
    pub login_cooldown_secs: u64,
    /// Consecutive account failures that open its circuit breaker; `0`
    /// disables the breaker.
    pub account_fail_threshold: u32,
    /// Ceiling for the breaker's exponential cooldown, in seconds.
    pub account_cooldown_max_secs: u64,
    /// Retries for a throttled turn, before the first byte. Python:
    /// `RATE_LIMIT_MAX_RETRIES`.
    pub rate_limit_max_retries: u32,
    /// Base backoff in seconds, multiplied by the attempt number. Python:
    /// `RATE_LIMIT_BACKOFF_S` (and it floors at 0.5s).
    pub rate_limit_backoff_secs: f64,
    /// Correction turns for malformed tool calls. Python:
    /// `TOOL_REPAIR_MAX_RETRIES`.
    pub tool_repair_max_retries: u32,
    /// Wall-clock budget for one turn, in seconds. Python:
    /// `REQUEST_TIMEOUT` (default 300), applied by `asyncio.timeout` around the
    /// whole turn. Without it a stalled upstream socket hangs the request
    /// forever and nothing is ever logged.
    pub request_timeout_secs: f64,
    /// How long in-flight turns may finish after SIGTERM, in seconds. Without
    /// a deadline a single stalled stream blocks shutdown until the CLI's
    /// SIGKILL.
    pub shutdown_grace_secs: u64,
    /// SSE keep-alive interval in seconds. Python: `HEARTBEAT_INTERVAL`
    /// (default 10). Clients and proxies drop an idle stream otherwise.
    pub heartbeat_interval_secs: f64,
    pub image_max_bytes: usize,
}

impl Default for LimitsSection {
    fn default() -> Self {
        Self {
            // Measured: sequential requests with no gap do not trip the upstream
            // rate limiter, so the safe default is no delay. Raise it if you see
            // `rate_limit_reached` / `Messages too frequent`.
            request_delay_secs: 0.0,
            queue_limit: 64,
            queue_timeout_secs: 120.0,
            login_cooldown_secs: 300,
            account_fail_threshold: 3,
            account_cooldown_max_secs: 600,
            rate_limit_max_retries: 3,
            rate_limit_backoff_secs: 10.0,
            tool_repair_max_retries: 3,
            request_timeout_secs: 300.0,
            shutdown_grace_secs: 20,
            heartbeat_interval_secs: 10.0,
            image_max_bytes: 10 * 1024 * 1024,
        }
    }
}

/// Fully resolved configuration.
#[derive(Debug, Clone)]
pub struct Config {
    pub host: String,
    pub port: u16,
    /// Selected wire dialect, resolved and validated at load time.
    pub dialect: crate::dialect::Dialect,
    pub db_path: String,
    pub model: String,
    pub thinking: bool,
    pub search: bool,
    pub disabled_accounts: DisabledAccounts,
    pub request_delay_secs: f64,
    pub queue_limit: usize,
    pub queue_timeout_secs: f64,
    pub login_cooldown_secs: u64,
    pub account_fail_threshold: u32,
    pub account_cooldown_max_secs: u64,
    pub rate_limit_max_retries: u32,
    pub rate_limit_backoff_secs: f64,
    pub tool_repair_max_retries: u32,
    pub request_timeout_secs: f64,
    pub shutdown_grace_secs: u64,
    pub heartbeat_interval_secs: f64,
    pub image_max_bytes: usize,
}

impl Config {
    /// Resolve config: `config.toml` (if present) then environment overrides.
    ///
    /// `CONFIG_PATH` selects a different file; a missing file is not an error.
    pub fn load() -> Result<Self, String> {
        let path = std::env::var("CONFIG_PATH").unwrap_or_else(|_| default_config_path());
        let file = Self::read_file(&path)?;

        let dialect_name = env_str("DIALECT").or(file.proxy.dialect);
        let dialect = match dialect_name {
            Some(name) => crate::dialect::Dialect::parse(&name)?,
            None => crate::dialect::Dialect::default(),
        };

        Ok(Self {
            host: env_str("PROXY_HOST").unwrap_or(file.proxy.host),
            port: env_num("PROXY_PORT").unwrap_or(file.proxy.port),
            dialect,
            db_path: env_str("DB_PATH")
                .map(PathBuf::from)
                .unwrap_or_else(|| anchor_path(&path, &file.storage.db_path))
                .to_string_lossy()
                .into_owned(),
            model: env_str("MODEL_NAME").unwrap_or(file.deepseek.model),
            thinking: env_flag("THINKING_ENABLED", file.deepseek.thinking),
            search: env_flag("SEARCH_ENABLED", file.deepseek.search),
            disabled_accounts: env_str("DEEPSEEK_DISABLED_ACCOUNTS")
                .map(|raw| DisabledAccounts::from_csv(&raw))
                .unwrap_or_else(|| DisabledAccounts::from_entries(&file.deepseek.disabled_accounts)),
            request_delay_secs: env_num("REQUEST_DELAY")
                .unwrap_or(file.limits.request_delay_secs)
                .max(0.0),
            queue_limit: env_num("QUEUE_LIMIT").unwrap_or(file.limits.queue_limit),
            queue_timeout_secs: env_num("QUEUE_TIMEOUT")
                .unwrap_or(file.limits.queue_timeout_secs)
                .max(1.0),
            login_cooldown_secs: env_num("LOGIN_COOLDOWN")
                .unwrap_or(file.limits.login_cooldown_secs),
            account_fail_threshold: env_num("ACCOUNT_FAIL_THRESHOLD")
                .unwrap_or(file.limits.account_fail_threshold),
            account_cooldown_max_secs: env_num("ACCOUNT_COOLDOWN_MAX")
                .unwrap_or(file.limits.account_cooldown_max_secs)
                .max(1),
            rate_limit_max_retries: env_num("RATE_LIMIT_MAX_RETRIES")
                .unwrap_or(file.limits.rate_limit_max_retries),
            rate_limit_backoff_secs: env_num("RATE_LIMIT_BACKOFF_S")
                .unwrap_or(file.limits.rate_limit_backoff_secs)
                .max(0.0),
            tool_repair_max_retries: env_num("TOOL_REPAIR_MAX_RETRIES")
                .unwrap_or(file.limits.tool_repair_max_retries),
            request_timeout_secs: env_num("REQUEST_TIMEOUT")
                .unwrap_or(file.limits.request_timeout_secs)
                .max(1.0),
            shutdown_grace_secs: env_num("SHUTDOWN_GRACE")
                .unwrap_or(file.limits.shutdown_grace_secs)
                .max(1),
            heartbeat_interval_secs: env_num("HEARTBEAT_INTERVAL")
                .unwrap_or(file.limits.heartbeat_interval_secs)
                .max(1.0),
            image_max_bytes: env_num("IMAGE_MAX_BYTES").unwrap_or(file.limits.image_max_bytes),
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

/// Resolve a relative path against the config file's directory, so a relative
/// `db_path` means "next to config.toml" rather than "wherever the process was
/// started". Absolute paths pass through untouched.
fn anchor_path(config_path: &str, value: &str) -> PathBuf {
    let p = PathBuf::from(value);
    if p.is_absolute() {
        return p;
    }
    match Path::new(config_path).parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.join(p),
        _ => p,
    }
}

/// Default `config.toml` location: the repo root this binary was built from,
/// not the process working directory. Running the server from `/` or from a
/// systemd unit must still pick up the checked-in config.
fn default_config_path() -> String {
    // Compiled-in repo root; `config.rs` lives at <root>/crates/proxy/src/.
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let candidate = root.join("config.toml");
    if candidate.exists() {
        return candidate.to_string_lossy().into_owned();
    }
    // Packaged/moved binary: fall back to the working directory.
    "config.toml".into()
}

fn env_str(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

fn env_num<T: std::str::FromStr>(key: &str) -> Option<T> {
    env_str(key).and_then(|v| v.trim().parse().ok())
}

/// One `disabled_accounts` entry: a 1-based database number or an email.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum DisabledAccount {
    Number(usize),
    Email(String),
}

/// Resolved `disabled_accounts`: database numbers plus email addresses.
///
/// Numbers address database accounts (1-based, creation order); emails address
/// any account, including one that only exists in `.env`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DisabledAccounts {
    pub numbers: Vec<usize>,
    pub emails: Vec<String>,
}

impl DisabledAccounts {
    pub fn is_empty(&self) -> bool {
        self.numbers.is_empty() && self.emails.is_empty()
    }

    pub fn contains_number(&self, number: usize) -> bool {
        self.numbers.contains(&number)
    }

    /// Case-insensitive: an email typed with different capitalisation must
    /// still disable the account it names.
    pub fn contains_email(&self, email: &str) -> bool {
        let email = email.trim();
        self.emails.iter().any(|e| e.eq_ignore_ascii_case(email))
    }

    /// Either kind of entry disables the account.
    pub fn contains(&self, number: usize, email: &str) -> bool {
        self.contains_number(number) || self.contains_email(email)
    }

    fn from_entries(entries: &[DisabledAccount]) -> Self {
        let mut out = Self::default();
        for entry in entries {
            match entry {
                DisabledAccount::Number(n) => out.numbers.push(*n),
                DisabledAccount::Email(e) => {
                    let e = e.trim().trim_matches('"');
                    if !e.is_empty() {
                        out.emails.push(e.to_string());
                    }
                }
            }
        }
        out.numbers.sort_unstable();
        out.numbers.dedup();
        out.emails.sort_unstable();
        out.emails.dedup();
        out
    }

    /// `"1,2"`, `"[1, 2]"`, `"a@b.com,c@d.com"`, or a mixture: anything that
    /// parses as a number is a database number, everything else an email.
    fn from_csv(raw: &str) -> Self {
        let entries: Vec<DisabledAccount> = raw
            .trim_matches(|c| c == '[' || c == ']')
            .split([',', ' ', ';'])
            .map(|t| t.trim().trim_matches('"'))
            .filter(|t| !t.is_empty())
            .map(|token| match token.parse::<usize>() {
                Ok(n) => DisabledAccount::Number(n),
                Err(_) => DisabledAccount::Email(token.to_string()),
            })
            .collect();
        Self::from_entries(&entries)
    }
}

/// Accounts disabled by configuration, cached for the process lifetime.
///
/// `load_accounts()` runs on startup, on `accounts list`, and on every config
/// reload; re-reading `config.toml` each time would be wasteful and could
/// disagree with itself mid-run. A config that fails to load means "nothing
/// disabled" here, because the caller that actually needs the config reports
/// the error itself.
pub fn disabled_accounts() -> &'static DisabledAccounts {
    static CACHE: OnceLock<DisabledAccounts> = OnceLock::new();
    CACHE.get_or_init(|| {
        Config::load()
            .map(|cfg| cfg.disabled_accounts)
            .unwrap_or_default()
    })
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
        assert_eq!(f.limits.request_delay_secs, 0.0);
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
    fn relative_db_path_is_anchored_to_the_config_dir() {
        let got = anchor_path("/srv/app/config.toml", "data/deepseek-rs.db");
        assert_eq!(got, PathBuf::from("/srv/app/data/deepseek-rs.db"));
        // Absolute values are left alone, whatever the config location.
        let abs = anchor_path("/srv/app/config.toml", "/var/lib/ds.db");
        assert_eq!(abs, PathBuf::from("/var/lib/ds.db"));
        // Bare filename with no directory component: never becomes absolute.
        assert_eq!(
            anchor_path("config.toml", "deepseek-rs.db"),
            PathBuf::from("deepseek-rs.db")
        );
    }

    #[test]
    fn disabled_accounts_accept_numbers_and_emails() {
        let f: FileConfig = toml::from_str(
            "[deepseek]\ndisabled_accounts = [2, 5, \"C@D.com\"]\n",
        )
        .unwrap();
        let d = DisabledAccounts::from_entries(&f.deepseek.disabled_accounts);
        assert_eq!(d.numbers, vec![2, 5]);
        assert_eq!(d.emails, vec!["C@D.com".to_string()]);
        assert!(d.contains_number(2));
        assert!(!d.contains_number(3));
        assert!(d.contains_email("c@d.com"));
        assert!(!d.contains_email("other@x.com"));
        assert!(d.contains(5, "nobody@x.com"));
        // Omitted means nothing disabled, never "disable everything".
        let empty: FileConfig = toml::from_str("").unwrap();
        assert!(DisabledAccounts::from_entries(&empty.deepseek.disabled_accounts).is_empty());
    }

    #[test]
    fn disabled_accounts_env_form_accepts_numbers_and_emails() {
        let d = DisabledAccounts::from_csv("1, a@b.com;3");
        assert_eq!(d.numbers, vec![1, 3]);
        assert_eq!(d.emails, vec!["a@b.com".to_string()]);
        assert!(DisabledAccounts::from_csv("  ").is_empty());
        assert!(DisabledAccounts::from_csv("[a@b.com]").contains_email("a@b.com"));
    }

    #[test]
    fn missing_file_is_not_an_error() {
        let f = Config::read_file("/nonexistent/config.toml").unwrap();
        assert_eq!(f.proxy.port, 9174);
    }

    #[test]
    fn dialect_defaults_to_json_invoke() {
        use crate::dialect::Dialect;
        let f: FileConfig = toml::from_str("").unwrap();
        assert_eq!(f.proxy.dialect, None);
        assert_eq!(Dialect::default(), Dialect::JsonInvoke);
    }

    #[test]
    fn dialect_key_is_read_from_the_proxy_section() {
        let f: FileConfig = toml::from_str("[proxy]\ndialect = \"deepseek_v4_dsml\"\n").unwrap();
        assert_eq!(f.proxy.dialect.as_deref(), Some("deepseek_v4_dsml"));
    }

    #[test]
    fn dialect_names_parse_and_unknown_is_a_hard_error() {
        use crate::dialect::Dialect;
        assert_eq!(
            Dialect::parse("json_invoke").unwrap(),
            Dialect::JsonInvoke
        );
        assert_eq!(
            Dialect::parse("deepseek_v4_dsml").unwrap(),
            Dialect::DeepSeekV4Dsml
        );
        assert_eq!(
            Dialect::parse("simple_tool_tag").unwrap(),
            Dialect::SimpleToolTag
        );
        let err = Dialect::parse("dsml").unwrap_err();
        assert!(
            err.contains("unknown dialect"),
            "unknown values must not fall back: {err}"
        );
    }
}
