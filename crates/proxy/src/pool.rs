//! Multi-account pool with per-account rate gating and failover.
//!
//! Port of `ConnectionPool` / `Connection` from the Python proxy:
//!
//! - Every account has its own gate. Only one request per account runs at a
//!   time, and the next one waits `request_delay` after the previous
//!   **finishes** (completion-to-start, not start-to-start).
//! - A thread binds to an account for its lifetime. New threads go to the
//!   least-crowded healthy account; the binding survives restarts via SQLite.
//! - A login-dead account enters cooldown and its threads fail over to the
//!   least-crowded remaining healthy account.

use client::Client;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, OwnedMutexGuard, RwLock};
use tokio::time::Instant;

#[derive(Debug)]
struct GateState {
    last_fire: Option<Instant>,
    cooldown_until: Option<Instant>,
}

/// Breaker bookkeeping, deliberately separate from the rate gate.
///
/// The gate mutex is held by the caller's [`RateTicket`] for the whole turn, so
/// anything updated *after* a turn (success, failure) cannot live behind it: it
/// would deadlock against the ticket the caller still owns. A plain `std` mutex
/// is right here because no critical section awaits.
#[derive(Debug, Default)]
struct BreakerState {
    /// Failures since the last success.
    failures: u32,
    /// When the breaker will consider a probe; `None` while closed.
    open_until: Option<Instant>,
    /// A probe has been handed to a request and has not reported back yet.
    probing: bool,
}

/// What `/stats` reports for one account's breaker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Breaker {
    Closed,
    Open,
    HalfOpen,
}

impl Breaker {
    pub fn as_str(self) -> &'static str {
        match self {
            Breaker::Closed => "closed",
            Breaker::Open => "open",
            Breaker::HalfOpen => "half_open",
        }
    }
}

/// Per-account rate gate and breaker settings, taken from `PoolConfig`.
///
/// Grouped so `Connection::new` takes the account and this struct rather than
/// eleven positional arguments.
#[derive(Debug, Clone, Copy)]
struct GateConfig {
    request_delay: Duration,
    queue_limit: usize,
    queue_timeout: Duration,
    fail_threshold: u32,
    cooldown_base: Duration,
    cooldown_max: Duration,
}

impl GateConfig {
    fn from_pool(config: &PoolConfig) -> Self {
        Self {
            request_delay: config.request_delay,
            queue_limit: config.queue_limit,
            queue_timeout: config.queue_timeout,
            fail_threshold: config.account_fail_threshold,
            cooldown_base: config.login_cooldown,
            cooldown_max: config.account_cooldown_max,
        }
    }
}

/// One DeepSeek account and its rate gate.
pub struct Connection {
    pub email: String,
    password: String,
    /// Explicitly configured token (`DEEPSEEK_TOKEN_<i>`); never overwritten.
    token: Option<String>,
    client: RwLock<Client>,
    gate: Arc<Mutex<GateState>>,
    breaker: std::sync::Mutex<BreakerState>,
    request_delay: Duration,
    waiting: AtomicUsize,
    queue_limit: usize,
    queue_timeout: Duration,
    /// Consecutive failures that open the breaker; `0` disables it.
    fail_threshold: u32,
    /// First cooldown when the breaker opens; doubles per extra failure.
    cooldown_base: Duration,
    /// Upper bound for the exponential breaker cooldown.
    cooldown_max: Duration,
    store: crate::store::Store,
}

impl Connection {
    fn new(
        email: String,
        password: String,
        token: Option<String>,
        client: Client,
        gate_config: GateConfig,
        store: crate::store::Store,
    ) -> Self {
        let GateConfig {
            request_delay,
            queue_limit,
            queue_timeout,
            fail_threshold,
            cooldown_base,
            cooldown_max,
        } = gate_config;
        Self {
            email,
            password,
            token,
            client: RwLock::new(client),
            gate: Arc::new(Mutex::new(GateState {
                last_fire: None,
                cooldown_until: None,
            })),
            breaker: std::sync::Mutex::new(BreakerState::default()),
            request_delay,
            waiting: AtomicUsize::new(0),
            queue_limit,
            queue_timeout,
            fail_threshold,
            cooldown_base,
            cooldown_max,
            store,
        }
    }

    pub async fn client(&self) -> Client {
        self.client.read().await.clone()
    }

    /// Wait for this account's rate slot. The returned ticket holds the slot;
    /// dropping it records the completion time and lets the next request in.
    pub async fn acquire(self: &Arc<Self>) -> Result<RateTicket, String> {
        // A genuinely open breaker must not queue: the caller has to try another
        // account instead of waiting out a cooldown that may be minutes long.
        // A plain rate-limit cooldown is different: the gate sleeps it out below.
        if self.breaker_open() {
            return Err(BREAKER_OPEN.to_string());
        }
        if self.waiting.load(Ordering::Relaxed) >= self.queue_limit {
            return Err("account request queue is full".into());
        }
        self.waiting.fetch_add(1, Ordering::Relaxed);

        let guard =
            match tokio::time::timeout(self.queue_timeout, self.gate.clone().lock_owned()).await {
                Ok(g) => g,
                Err(_) => {
                    self.waiting.fetch_sub(1, Ordering::Relaxed);
                    return Err(format!(
                        "account queue timeout after {}s",
                        self.queue_timeout.as_secs_f64()
                    ));
                }
            };
        self.waiting.fetch_sub(1, Ordering::Relaxed);

        let now = Instant::now();
        let deadline = [
            guard.last_fire.map(|t| t + self.request_delay),
            guard.cooldown_until,
        ]
        .into_iter()
        .flatten()
        .max();

        if let Some(deadline) = deadline {
            let wait = deadline.saturating_duration_since(now);
            if !wait.is_zero() {
                tokio::time::sleep(wait).await;
            }
        }

        // The breaker may have opened while this request waited its turn, and a
        // half-open account hands out exactly one probe.
        if !self.claim(Instant::now()) {
            return Err(BREAKER_OPEN.to_string());
        }

        Ok(RateTicket {
            guard: Some(guard),
            account: self.email.clone(),
        })
    }

    /// Hold this account out of service for `cooldown` without moving threads.
    ///
    /// The rate-limit backoff uses this so the gate itself sleeps: a second
    /// timer would race `acquire()` and let the retry fire too early.
    pub async fn cool_down(&self, cooldown: Duration) {
        self.penalize(cooldown).await;
    }

    /// Count one failed attempt, opening the breaker at the threshold.
    ///
    /// The cooldown doubles with each consecutive failure (`base * 2^k`) and is
    /// capped by `cooldown_max`, so a flapping account is retried less and less
    /// often instead of on every turn.
    fn record_failure(&self) {
        let mut b = self.breaker_state();
        b.failures = b.failures.saturating_add(1);
        b.probing = false;
        if self.fail_threshold == 0 || b.failures < self.fail_threshold {
            return;
        }
        let exponent = (b.failures - self.fail_threshold).min(8);
        let delay = (self.cooldown_base * 2u32.pow(exponent)).min(self.cooldown_max);
        let until = Instant::now() + delay;
        b.open_until = Some(match b.open_until {
            Some(existing) if existing > until => existing,
            _ => until,
        });
    }

    /// A completed turn proves the account works: close the breaker.
    fn record_success(&self) {
        *self.breaker_state() = BreakerState::default();
    }

    /// Breaker state at `now`, and how long until a probe is allowed.
    fn breaker_at(b: &BreakerState, threshold: u32, now: Instant) -> (Breaker, Option<Duration>) {
        match b.open_until {
            Some(until) if until > now => (Breaker::Open, Some(until - now)),
            Some(_) if threshold > 0 && b.failures >= threshold => {
                (Breaker::HalfOpen, Some(Duration::ZERO))
            }
            _ => (Breaker::Closed, None),
        }
    }

    fn breaker_state(&self) -> std::sync::MutexGuard<'_, BreakerState> {
        self.breaker.lock().expect("breaker lock poisoned")
    }

    /// Is the breaker open right now?
    ///
    /// Only the breaker is consulted: a rate-limit cooldown is slept out by
    /// [`Connection::acquire`], not turned into an account failure.
    fn breaker_open(&self) -> bool {
        let b = self.breaker_state();
        Self::breaker_at(&b, self.fail_threshold, Instant::now()).0 == Breaker::Open
    }

    /// Re-check the breaker once the gate is held, claiming the half-open
    /// probe. Returns `false` when this request must go to another account.
    fn claim(&self, now: Instant) -> bool {
        let mut b = self.breaker_state();
        match Self::breaker_at(&b, self.fail_threshold, now).0 {
            Breaker::Closed => true,
            Breaker::Open => false,
            Breaker::HalfOpen => {
                if b.probing {
                    return false;
                }
                b.probing = true;
                true
            }
        }
    }

    /// Mark this account unhealthy for `cooldown`.
    async fn penalize(&self, cooldown: Duration) {
        let mut g = self.gate.lock().await;
        let until = Instant::now() + cooldown;
        g.cooldown_until = Some(match g.cooldown_until {
            Some(existing) if existing > until => existing,
            _ => until,
        });
    }

    /// Point-in-time view of this account for `stats`.
    pub async fn snapshot(&self, bound_threads: usize) -> ConnectionSnapshot {
        let now = Instant::now();
        let (breaker, retry_in) = {
            let b = self.breaker_state();
            Self::breaker_at(&b, self.fail_threshold, now)
        };
        let g = self.gate.lock().await;
        let cooldown_ms = g
            .cooldown_until
            .filter(|u| *u > now)
            .map(|u| u.saturating_duration_since(now).as_millis() as u64);
        ConnectionSnapshot {
            email: self.email.clone(),
            healthy: cooldown_ms.is_none() && breaker != Breaker::Open,
            bound_threads,
            waiting: self.waiting.load(Ordering::Relaxed),
            cooldown_ms,
            breaker,
            retry_in_secs: retry_in.map(|d| d.as_secs_f64().ceil() as u64),
        }
    }

    /// Cheap gate check used by routing; does not claim a probe.
    fn healthy(&self) -> bool {
        !self.breaker_open()
    }

    /// Re-login, replacing the client. Resume ids stay valid.
    ///
    /// Any cached token is dropped first: this path is only reached after the
    /// server rejected a token, so reusing it would loop. A configured token
    /// is honoured when no password is available, but a password login wins
    /// because it yields a fresh token.
    async fn reconnect(&self) -> Result<(), String> {
        let _ = self.store.forget_token(&self.email);
        let fresh = self.login(None).await.map_err(|e| e.to_string())?;
        *self.client.write().await = fresh;
        Ok(())
    }

    /// Build a client from `cached`, or log in and persist the new token.
    async fn login(&self, cached: Option<String>) -> Result<Client, String> {
        if let Some(cached) = cached {
            tracing::debug!("account {}: reusing cached token", self.email);
            return Ok(Client::from_token(cached));
        }
        if !self.password.is_empty() {
            let client = Client::login(&self.email, &self.password, client::get_device_id())
                .await
                .map_err(|e| e.to_string())?;
            if let Err(e) = self.store.save_token(&self.email, client.token()) {
                tracing::warn!("account {}: could not cache token: {e}", self.email);
            }
            return Ok(client);
        }
        match &self.token {
            Some(token) => Ok(Client::from_token(token.clone())),
            None => Err(format!("account {}: no password or token", self.email)),
        }
    }

    /// Initial login: configured token > cached token > password login.
    async fn initial_login(&self) -> Result<Client, String> {
        if let Some(token) = &self.token {
            return Ok(Client::from_token(token.clone()));
        }
        let cached = self
            .store
            .get_token(&self.email)
            .ok()
            .flatten()
            .filter(|t| !t.is_empty());
        self.login(cached).await
    }
}

/// One account's state, as reported by `GET /stats`.
#[derive(Debug, Clone)]
pub struct ConnectionSnapshot {
    pub email: String,
    pub healthy: bool,
    pub bound_threads: usize,
    pub waiting: usize,
    pub cooldown_ms: Option<u64>,
    pub breaker: Breaker,
    /// Seconds until a half-open probe is allowed; `None` when closed.
    pub retry_in_secs: Option<u64>,
}

/// Holds an account's rate slot; release is implicit on drop.
pub struct RateTicket {
    guard: Option<OwnedMutexGuard<GateState>>,
    /// Account that served this turn. The caller logs it once per completed
    /// turn; the ticket is the only place that knows which one won the route.
    pub account: String,
}

impl Drop for RateTicket {
    fn drop(&mut self) {
        if let Some(mut g) = self.guard.take() {
            g.last_fire = Some(Instant::now());
        }
    }
}

/// Pool tunables. `PartialEq` so a reload can tell whether anything in this
/// group actually changed before paying for a rebuild (one login per account).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PoolConfig {
    pub request_delay: Duration,
    pub queue_limit: usize,
    /// Max time to wait for a busy account's rate slot. Python: `QUEUE_TIMEOUT`.
    pub queue_timeout: Duration,
    pub login_cooldown: Duration,
    /// Consecutive failures that open an account's breaker; `0` disables it.
    pub account_fail_threshold: u32,
    /// Ceiling for the exponential breaker cooldown.
    pub account_cooldown_max: Duration,
}

impl PoolConfig {
    /// Project the pool tunables out of the resolved config.
    pub fn from_config(cfg: &crate::config::Config) -> Self {
        Self {
            request_delay: Duration::from_secs_f64(cfg.request_delay_secs),
            queue_limit: cfg.queue_limit,
            queue_timeout: Duration::from_secs_f64(cfg.queue_timeout_secs),
            login_cooldown: Duration::from_secs(cfg.login_cooldown_secs),
            account_fail_threshold: cfg.account_fail_threshold,
            account_cooldown_max: Duration::from_secs(cfg.account_cooldown_max_secs),
        }
    }
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            request_delay: Duration::ZERO,
            queue_limit: 64,
            queue_timeout: Duration::from_secs(120),
            login_cooldown: Duration::from_secs(300),
            account_fail_threshold: 3,
            account_cooldown_max: Duration::from_secs(600),
        }
    }
}

pub struct Pool {
    conns: Vec<Arc<Connection>>,
    owner: Mutex<HashMap<String, usize>>,
    config: PoolConfig,
    store: crate::store::Store,
    /// Every configured email, including ones that failed to log in at boot.
    /// A reload compares this against the newly configured list to decide
    /// whether accounts changed; comparing live `conns` would rebuild on every
    /// SIGHUP whenever one account is dead.
    configured_emails: Vec<String>,
}

/// One configured account before login.
pub struct AccountSpec {
    pub email: String,
    pub password: String,
    pub token: Option<String>,
}

impl Pool {
    pub async fn connect(
        accounts: Vec<AccountSpec>,
        config: PoolConfig,
        store: crate::store::Store,
    ) -> Result<Self, String> {
        if accounts.is_empty() {
            return Err("no DeepSeek accounts configured".into());
        }
        let configured_emails: Vec<String> = accounts.iter().map(|a| a.email.clone()).collect();
        let mut conns = Vec::new();
        for a in accounts {
            let conn = Connection::new(
                a.email,
                a.password,
                a.token,
                // Placeholder; replaced by `initial_login` below, which needs
                // the connection's own store handle for the token cache.
                Client::from_token(String::new()),
                GateConfig::from_pool(&config),
                store.clone(),
            );
            // A dead account must not take the whole server down: skip it and
            // keep the healthy ones, so one bad credential cannot prevent the
            // proxy from booting with the accounts that still work.
            match conn.initial_login().await {
                Ok(client) => {
                    *conn.client.write().await = client;
                    conns.push(Arc::new(conn));
                }
                Err(e) => tracing::warn!("skipping account {}: {e}", conn.email),
            }
        }
        if conns.is_empty() {
            return Err("no DeepSeek account could log in".into());
        }
        Ok(Self {
            conns,
            owner: Mutex::new(HashMap::new()),
            config,
            store,
            configured_emails,
        })
    }

    pub fn len(&self) -> usize {
        self.conns.len()
    }

    /// The tunables this pool was built with, for a reload to diff against.
    pub fn config(&self) -> &PoolConfig {
        &self.config
    }

    /// Emails from the configuration, including any that could not log in.
    pub fn configured_emails(&self) -> &[String] {
        &self.configured_emails
    }

    /// First account, without binding a thread to it. Used for health probes
    /// that must not leave a fake thread in the database.
    pub fn any(&self) -> Arc<Connection> {
        self.conns[0].clone()
    }

    #[allow(dead_code)] // used by tests and future admin UI
    pub fn is_empty(&self) -> bool {
        self.conns.is_empty()
    }

    /// Bind a thread to an account: honour an existing/persisted binding, else
    /// assign the least-crowded healthy account.
    pub async fn route(&self, thread_id: &str) -> Arc<Connection> {
        let mut stale_binding = None;
        {
            let owner = self.owner.lock().await;
            if let Some(&idx) = owner.get(thread_id) {
                if self.conns[idx].healthy() {
                    return self.conns[idx].clone();
                }
                // The bound account opened its breaker or is cooling down. Move
                // the thread to a working one instead of serving it a 503, but
                // do not forget the binding yet: if nothing else is healthy the
                // thread keeps its account and waits.
                stale_binding = Some(idx);
            }
        }

        // Persisted binding (survives restart) — unknown emails fall through.
        let bound = self
            .store
            .get_binding(thread_id)
            .ok()
            .flatten()
            .and_then(|email| self.conns.iter().position(|c| c.email == email));

        // A stale in-memory binding moves only if some other account works.
        let chosen = match stale_binding {
            Some(idx) => match self.least_crowded_healthy().await {
                Some(next) if next != idx => next,
                _ => return self.conns[idx].clone(),
            },
            None => match bound {
                Some(idx) => idx,
                None => self.least_crowded().await,
            },
        };

        self.owner
            .lock()
            .await
            .insert(thread_id.to_string(), chosen);
        if bound.is_none() || stale_binding.is_some() {
            let _ = self.store.bind(thread_id, &self.conns[chosen].email);
        }
        self.conns[chosen].clone()
    }

    /// Least-crowded account whose breaker is closed (or half-open), if any.
    async fn least_crowded_healthy(&self) -> Option<usize> {
        let counts = self.occupancy().await;
        let mut best = None;
        let mut best_score = usize::MAX;
        for (i, c) in self.conns.iter().enumerate() {
            if !c.healthy() {
                continue;
            }
            let score = counts.get(&c.email).copied().unwrap_or(0);
            if score < best_score {
                best_score = score;
                best = Some(i);
            }
        }
        best
    }

    /// Least-crowded account, preferring a healthy one.
    ///
    /// When every account is open (a total upstream outage) the least-crowded
    /// one is still used: refusing every request would turn a slow recovery
    /// into a dead proxy, and the breaker keeps failing fast anyway.
    async fn least_crowded(&self) -> usize {
        if let Some(idx) = self.least_crowded_healthy().await {
            return idx;
        }
        tracing::warn!("every account has an open circuit breaker; trying the least busy");
        let counts = self.occupancy().await;
        let mut best = 0usize;
        let mut best_score = usize::MAX;
        for (i, c) in self.conns.iter().enumerate() {
            let score = counts.get(&c.email).copied().unwrap_or(0);
            if score < best_score {
                best_score = score;
                best = i;
            }
        }
        best
    }

    async fn occupancy(&self) -> HashMap<String, usize> {
        let owner = self.owner.lock().await;
        let mut counts: HashMap<String, usize> = HashMap::new();
        for &idx in owner.values() {
            *counts.entry(self.conns[idx].email.clone()).or_default() += 1;
        }
        counts
    }

    /// Move a thread off a login-dead account. Returns the replacement.
    pub async fn failover(&self, thread_id: &str) -> Option<Arc<Connection>> {
        let current = {
            let owner = self.owner.lock().await;
            owner.get(thread_id).copied()
        };
        if let Some(idx) = current {
            self.conns[idx].penalize(self.config.login_cooldown).await;
            self.conns[idx].record_failure();
        }
        self.owner.lock().await.remove(thread_id);

        let healthy: Vec<usize> = {
            let mut v = Vec::new();
            for (i, c) in self.conns.iter().enumerate() {
                if Some(i) != current && c.healthy() {
                    v.push(i);
                }
            }
            v
        };
        if healthy.is_empty() {
            return None;
        }

        let counts = self.occupancy().await;
        let chosen = *healthy
            .iter()
            .min_by_key(|&&i| counts.get(&self.conns[i].email).copied().unwrap_or(0))?;

        self.owner
            .lock()
            .await
            .insert(thread_id.to_string(), chosen);
        let _ = self.store.bind(thread_id, &self.conns[chosen].email);
        Some(self.conns[chosen].clone())
    }

    /// Per-account state plus how many threads each one owns.
    pub async fn snapshot(&self) -> Vec<ConnectionSnapshot> {
        let counts = self.occupancy().await;
        let mut out = Vec::with_capacity(self.conns.len());
        for c in &self.conns {
            let bound = counts.get(&c.email).copied().unwrap_or(0);
            out.push(c.snapshot(bound).await);
        }
        out
    }

    /// Try to re-login an account (invalid/expired token recovery).
    pub async fn recover(&self, conn: &Arc<Connection>) -> bool {
        conn.reconnect().await.is_ok()
    }
}

/// Why a completion attempt failed, in the caller's vocabulary.
///
/// The classifiers (`is_login_dead`, `is_rate_limit`, ...) and the account
/// objects stay inside this module: a caller never sees a [`Connection`], a
/// [`RateTicket`] or a raw reqwest error string.
#[derive(Debug)]
pub enum PoolError {
    /// No slot: queue full or queue wait timed out.
    Queue(String),
    /// The account's login is dead and no healthy account was left.
    LoginDead(String),
    /// Still throttled after every retry.
    RateLimited {
        retry_after_s: u64,
    },
    Other(String),
}

impl std::fmt::Display for PoolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PoolError::Queue(m) | PoolError::LoginDead(m) | PoolError::Other(m) => f.write_str(m),
            PoolError::RateLimited { retry_after_s } => write!(
                f,
                "DeepSeek backend rate-limited this turn (retry after {retry_after_s}s)"
            ),
        }
    }
}

impl Pool {
    /// Open a completion stream for `thread_id`, retrying, cooling down and
    /// failing over inside this module.
    ///
    /// Returns the live response together with the rate ticket, which the
    /// caller must hold until the body is drained: the next request on the
    /// account starts `request_delay` after the stream really finished.
    ///
    /// `on_retry(attempt, wait_secs)` is called before each retry so the caller
    /// can count and log it without knowing why the retry happened.
    pub async fn complete(
        &self,
        thread_id: &str,
        body: &client::CompletionBody,
        max_retries: u32,
        backoff_secs: f64,
        mut on_retry: impl FnMut(u32, f64),
    ) -> Result<(reqwest::Response, RateTicket), PoolError> {
        let backoff = backoff_secs.max(0.5);
        let mut attempt = 0u32;
        let mut conn = self.route(thread_id).await;

        loop {
            let slot = match conn.acquire().await {
                Ok(slot) => slot,
                Err(e) if e == BREAKER_OPEN => {
                    // The breaker opened between routing and this attempt.
                    drop(conn);
                    match self.failover(thread_id).await {
                        Some(next) => {
                            tracing::warn!("thread failed over to {} (breaker)", next.email);
                            conn = next;
                            continue;
                        }
                        None => return Err(PoolError::Other(e)),
                    }
                }
                Err(e) => return Err(PoolError::Queue(e)),
            };
            let client = conn.client().await;

            match client.stream_completion(body).await {
                Ok(resp) => {
                    conn.record_success();
                    return Ok((resp, slot));
                }
                Err(e) => {
                    let last = e.to_string();

                    if is_login_dead(&last) {
                        tracing::warn!("account {} login-dead: {last}", conn.email);
                        drop(slot);
                        match self.failover(thread_id).await {
                            Some(next) => {
                                tracing::warn!("thread failed over to {}", next.email);
                                conn = next;
                                continue;
                            }
                            None => return Err(PoolError::LoginDead(last)),
                        }
                    }

                    if is_invalid_token(&last) && self.recover(&conn).await {
                        tracing::info!("recovered account {}", conn.email);
                        drop(slot);
                        continue;
                    }

                    if !is_rate_limit(&last) {
                        // A 5xx, a timeout or a transport error counts against
                        // the account; a client-side 4xx never reaches here.
                        conn.record_failure();
                        drop(slot);
                        return Err(PoolError::Other(last));
                    }

                    let step = (attempt + 1).min(max_retries.max(1)) as f64;
                    let wait = backoff * step;
                    drop(slot);
                    conn.cool_down(Duration::from_secs_f64(wait)).await;

                    if attempt < max_retries {
                        attempt += 1;
                        on_retry(attempt, wait);
                        continue;
                    }

                    return Err(PoolError::RateLimited {
                        retry_after_s: wait.ceil() as u64,
                    });
                }
            }
        }
    }
}

/// Error text used when a request reaches an account whose breaker is open.
const BREAKER_OPEN: &str = "account circuit breaker is open";

/// True when the error looks like a dead login rather than a transient blip.
pub fn is_login_dead(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    [
        "login failed",
        "risk_device",
        "risk device",
        "biz_code=11",
        "unauthorized",
        "auth failed",
        "authentication failed",
    ]
    .iter()
    .any(|k| m.contains(k))
}

pub fn is_invalid_token(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("invalidtoken") || m.contains("invalid token") || m.contains("40003")
}

/// Markers that mean "throttled, come back later" rather than "broken".
///
/// Verbatim from `_RATE_LIMIT_MARKERS` in `deepseek_proxy/backend.py`, including
/// the deliberately broad `busy`/`too many` entries the Python proxy shipped.
const RATE_LIMIT_MARKERS: [&str; 10] = [
    "rate_limit",
    "rate limit",
    "too frequent",
    "too many",
    "429",
    "throttl",
    "overloaded",
    "busy",
    "generation_err",
    "server is temporarily unavailable",
];

/// True when the error is a throttle, i.e. retrying with backoff can work.
pub fn is_rate_limit(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    RATE_LIMIT_MARKERS.iter().any(|k| m.contains(k))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_throttles_not_auth() {
        assert!(is_rate_limit(
            "upstream hint error: Messages too frequent. Try again later."
        ));
        assert!(is_rate_limit("rate_limit_reached"));
        assert!(!is_rate_limit("invalid token"));
        assert!(!is_rate_limit("login failed: risk_device"));
    }

    #[test]
    fn rate_limit_markers_do_not_claim_login_errors() {
        for msg in ["login failed", "risk_device", "authentication failed"] {
            assert!(!is_rate_limit(msg), "{msg} must be a login error");
            assert!(is_login_dead(msg));
        }
    }

    fn gate(failures: u32, open_until: Option<Instant>, probing: bool) -> BreakerState {
        BreakerState {
            failures,
            open_until,
            probing,
        }
    }

    #[test]
    fn breaker_opens_at_the_threshold_and_recovers_after_the_window() {
        let now = Instant::now();
        // Below the threshold the breaker is still closed.
        assert_eq!(
            Connection::breaker_at(&gate(2, None, false), 3, now).0,
            Breaker::Closed
        );
        // At the threshold with an open window: open, and it reports the wait.
        let (state, retry) = Connection::breaker_at(&gate(3, Some(now + Duration::from_secs(5)), false), 3, now);
        assert_eq!(state, Breaker::Open);
        assert_eq!(retry, Some(Duration::from_secs(5)));
        // Window elapsed: half-open, ready for one probe.
        let (state, _) = Connection::breaker_at(&gate(3, Some(now), false), 3, now);
        assert_eq!(state, Breaker::HalfOpen);
        // Threshold 0 disables the breaker entirely.
        assert_eq!(
            Connection::breaker_at(&gate(9, Some(now), false), 0, now).0,
            Breaker::Closed
        );
    }

    #[test]
    fn breaker_state_names_are_stable_for_stats() {
        assert_eq!(Breaker::Closed.as_str(), "closed");
        assert_eq!(Breaker::Open.as_str(), "open");
        assert_eq!(Breaker::HalfOpen.as_str(), "half_open");
    }

    #[test]
    fn pool_error_display_is_the_client_visible_text() {
        assert_eq!(
            PoolError::RateLimited { retry_after_s: 7 }.to_string(),
            "DeepSeek backend rate-limited this turn (retry after 7s)"
        );
        assert_eq!(PoolError::Queue("full".into()).to_string(), "full");
    }

    #[test]
    fn classifiers_stay_inside_the_module() {
        assert!(is_login_dead("login failed for account"));
        assert!(is_rate_limit("server is temporarily unavailable"));
        assert!(is_invalid_token("invalid token"));
        assert!(!is_rate_limit("some other error"));
    }
}
