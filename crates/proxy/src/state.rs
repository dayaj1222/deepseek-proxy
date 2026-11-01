//! Process-wide state: the live settings snapshot, the account pool handle,
//! the thread cache and the counters behind `GET /stats`.
//!
//! Everything here is shared by every request, so every field is cheap to
//! clone (an `Arc`, a lock guard or an atomic). Nothing here awaits: a lock held
//! across an await is how a proxy deadlocks.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use crate::{config, dialect, pool, store, threads};

#[derive(Clone)]
pub(crate) struct Settings {
    pub(crate) thinking_default: bool,
    pub(crate) search_enabled: bool,
    pub(crate) model_name: String,
    /// Throttle retries before the first byte (`RATE_LIMIT_MAX_RETRIES`).
    pub(crate) rate_limit_max_retries: u32,
    /// Base backoff (`RATE_LIMIT_BACKOFF_S`), floored at 0.5s like Python.
    pub(crate) rate_limit_backoff_secs: f64,
    /// Correction turns for malformed tool calls (`TOOL_REPAIR_MAX_RETRIES`).
    pub(crate) tool_repair_max_retries: u32,
    /// Periodic tool-call reminder every N exchanges, 0 = off
    /// (`TOOL_REMINDER_INTERVAL`).
    pub(crate) tool_reminder_interval: u32,
    /// Whole-turn deadline in seconds (`REQUEST_TIMEOUT`).
    pub(crate) request_timeout_secs: f64,
    /// Largest accepted image in bytes (`IMAGE_MAX_BYTES`).
    pub(crate) image_max_bytes: usize,
    /// SSE keep-alive interval in seconds (`HEARTBEAT_INTERVAL`).
    pub(crate) heartbeat_interval_secs: f64,
    /// Wire dialect selected at startup (`dialect` / `DIALECT`).
    pub(crate) dialect: dialect::Dialect,
}

impl Settings {
    /// Project the reloadable subset of the resolved config.
    ///
    /// Restart-only keys (`host`, `port`, `db_path`, `image_max_bytes`) are
    /// still copied here because a running process reads them from somewhere,
    /// but a reload cannot change them; see [`restart_only_keys`].
    pub(crate) fn from_config(cfg: &config::Config) -> Self {
        Self {
            thinking_default: cfg.thinking,
            search_enabled: cfg.search,
            model_name: cfg.model.clone(),
            rate_limit_max_retries: cfg.rate_limit_max_retries,
            rate_limit_backoff_secs: cfg.rate_limit_backoff_secs,
            tool_repair_max_retries: cfg.tool_repair_max_retries,
            tool_reminder_interval: cfg.tool_reminder_interval,
            request_timeout_secs: cfg.request_timeout_secs,
            image_max_bytes: cfg.image_max_bytes,
            heartbeat_interval_secs: cfg.heartbeat_interval_secs,
            dialect: cfg.dialect,
        }
    }
}

#[derive(Clone)]
pub(crate) struct AppState {
    /// Live knobs. Behind a handle so a SIGHUP can swap them without a restart
    /// or a lock held across an await.
    pub(crate) settings: Arc<SettingsHandle>,
    /// Account pool. Swappable so a reload can add or remove an account without
    /// restarting; callers go through [`AppState::pool`] and never hold the lock
    /// across an await, so an in-flight turn finishes on the pool it started on.
    pub(crate) pool: Arc<std::sync::RwLock<Arc<pool::Pool>>>,
    /// Cache, turn lock and persistence for conversation threads.
    pub(crate) threads: Arc<threads::Threads>,
    pub(crate) store: store::Store,
    pub(crate) stats: Arc<Stats>,
}

impl AppState {
    /// Snapshot the current pool. One Arc clone; safe to hold for a whole turn.
    pub(crate) fn pool(&self) -> Arc<pool::Pool> {
        self.pool.read().expect("pool lock poisoned").clone()
    }

    /// Install a rebuilt pool. Turns already running keep their snapshot.
    pub(crate) fn set_pool(&self, fresh: Arc<pool::Pool>) {
        *self.pool.write().expect("pool lock poisoned") = fresh;
    }
}

/// The current `Settings`, swappable at runtime.
///
/// A turn takes one [`get`](Self::get) snapshot at its start and uses it for its
/// whole life: changing a turn's deadline halfway through a stream would be a
/// bug, not a feature. New turns see new values; running turns finish on the
/// values they started with.
pub(crate) struct SettingsHandle {
    current: std::sync::RwLock<Arc<Settings>>,
    /// The resolved config behind `current`, kept so a reload can tell a
    /// restart-only key apart from a live one.
    config: std::sync::Mutex<config::Config>,
    /// Bumped on every successful reload; 1 means "as loaded at boot".
    generation: AtomicU64,
    /// When the running config was installed.
    loaded_at: std::sync::Mutex<Instant>,
    /// Last reload failure. `None` means the running config is the last good
    /// one and nothing has failed since.
    last_error: std::sync::Mutex<Option<String>>,
}

/// Keys that cannot change while the process is running.
///
/// `host`/`port` are already bound, `db_path` is an open SQLite handle, and
/// `image_max_bytes` is baked into `DefaultBodyLimit` when the router is built.
/// A reload reports these instead of pretending to apply them.
pub(crate) fn restart_only_keys(old: &config::Config, new: &config::Config) -> Vec<&'static str> {
    let mut changed = Vec::new();
    if old.host != new.host || old.port != new.port {
        changed.push("host/port");
    }
    if old.db_path != new.db_path {
        changed.push("db_path");
    }
    if old.image_max_bytes != new.image_max_bytes {
        changed.push("image_max_bytes");
    }
    if old.dialect != new.dialect {
        changed.push("dialect");
    }
    changed
}

impl SettingsHandle {
    pub(crate) fn new(config: config::Config) -> Self {
        Self {
            current: std::sync::RwLock::new(Arc::new(Settings::from_config(&config))),
            config: std::sync::Mutex::new(config),
            generation: AtomicU64::new(1),
            loaded_at: std::sync::Mutex::new(Instant::now()),
            last_error: std::sync::Mutex::new(None),
        }
    }

    /// Install a new config. Running turns keep the snapshot they took.
    pub(crate) fn replace(&self, config: config::Config) -> u64 {
        let settings = Settings::from_config(&config);
        *self.current.write().expect("settings lock poisoned") = Arc::new(settings);
        *self.config.lock().expect("settings lock poisoned") = config;
        *self.loaded_at.lock().expect("settings lock poisoned") = Instant::now();
        *self.last_error.lock().expect("settings lock poisoned") = None;
        self.generation.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Record a failed reload. The running config stays untouched.
    pub(crate) fn note_error(&self, error: String) {
        *self.last_error.lock().expect("settings lock poisoned") = Some(error);
    }

    /// `(generation, seconds_since_last_successful_load, last_error)`.
    pub(crate) fn snapshot(&self) -> (u64, u64, Option<String>) {
        let last_error = self
            .last_error
            .lock()
            .expect("settings lock poisoned")
            .clone();
        (
            self.generation.load(Ordering::Relaxed),
            self.loaded_at
                .lock()
                .expect("settings lock poisoned")
                .elapsed()
                .as_secs(),
            last_error,
        )
    }

    /// Restart-only keys that changed since the running config was loaded.
    pub(crate) fn restart_only_changes(&self, new: &config::Config) -> Vec<&'static str> {
        let old = self.config.lock().expect("settings lock poisoned");
        restart_only_keys(&old, new)
    }

    /// Cheap: one Arc clone under a read lock, never held across an await.
    pub(crate) fn get(&self) -> Arc<Settings> {
        self.current.read().expect("settings lock poisoned").clone()
    }
}

/// Process counters reported by `GET /stats`. Cheap atomics only; anything
/// expensive (account gates, db counts) is read live by the handler.
#[derive(Default)]
pub(crate) struct Stats {
    started: Option<Instant>,
    pub(crate) total: AtomicU64,
    pub(crate) ok: AtomicU64,
    pub(crate) errors: AtomicU64,
    pub(crate) auth_failed: AtomicU64,
    pub(crate) retries: AtomicU64,
    pub(crate) tool_calls: AtomicU64,
    /// Images dropped because a turn carried more than DeepSeek accepts.
    pub(crate) images_dropped: AtomicU64,
    /// Set when SIGTERM/SIGINT arrives; `/readyz` reports 503 from then on.
    pub(crate) shutting_down: AtomicBool,
}

impl Stats {
    pub(crate) fn new() -> Self {
        Self {
            started: Some(Instant::now()),
            ..Default::default()
        }
    }

    pub(crate) fn uptime_secs(&self) -> u64 {
        self.started.map(|t| t.elapsed().as_secs()).unwrap_or(0)
    }
}
