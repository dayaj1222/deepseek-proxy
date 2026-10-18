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

/// One DeepSeek account and its rate gate.
pub struct Connection {
    pub email: String,
    password: String,
    /// Explicitly configured token (`DEEPSEEK_TOKEN_<i>`); never overwritten.
    token: Option<String>,
    client: RwLock<Client>,
    gate: Arc<Mutex<GateState>>,
    request_delay: Duration,
    waiting: AtomicUsize,
    queue_limit: usize,
    store: crate::store::Store,
}

impl Connection {
    fn new(
        email: String,
        password: String,
        token: Option<String>,
        client: Client,
        request_delay: Duration,
        queue_limit: usize,
        store: crate::store::Store,
    ) -> Self {
        Self {
            email,
            password,
            token,
            client: RwLock::new(client),
            gate: Arc::new(Mutex::new(GateState {
                last_fire: None,
                cooldown_until: None,
            })),
            request_delay,
            waiting: AtomicUsize::new(0),
            queue_limit,
            store,
        }
    }

    pub async fn client(&self) -> Client {
        self.client.read().await.clone()
    }

    /// Wait for this account's rate slot. The returned ticket holds the slot;
    /// dropping it records the completion time and lets the next request in.
    pub async fn acquire(self: &Arc<Self>) -> Result<RateTicket, String> {
        if self.waiting.load(Ordering::Relaxed) >= self.queue_limit {
            return Err("account request queue is full".into());
        }
        self.waiting.fetch_add(1, Ordering::Relaxed);

        let guard = self.gate.clone().lock_owned().await;
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

        Ok(RateTicket {
            guard: Some(guard),
        })
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

    async fn healthy(&self) -> bool {
        let g = self.gate.lock().await;
        g.cooldown_until.map(|u| u <= Instant::now()).unwrap_or(true)
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

/// Holds an account's rate slot; release is implicit on drop.
pub struct RateTicket {
    guard: Option<OwnedMutexGuard<GateState>>,
}

impl Drop for RateTicket {
    fn drop(&mut self) {
        if let Some(mut g) = self.guard.take() {
            g.last_fire = Some(Instant::now());
        }
    }
}

pub struct PoolConfig {
    pub request_delay: Duration,
    pub queue_limit: usize,
    pub login_cooldown: Duration,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            request_delay: Duration::from_secs(2),
            queue_limit: 64,
            login_cooldown: Duration::from_secs(300),
        }
    }
}

pub struct Pool {
    conns: Vec<Arc<Connection>>,
    owner: Mutex<HashMap<String, usize>>,
    config: PoolConfig,
    store: crate::store::Store,
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
        let mut conns = Vec::new();
        for a in accounts {
            let conn = Connection::new(
                a.email,
                a.password,
                a.token,
                // Placeholder; replaced by `initial_login` below, which needs
                // the connection's own store handle for the token cache.
                Client::from_token(String::new()),
                config.request_delay,
                config.queue_limit,
                store.clone(),
            );
            let client = conn
                .initial_login()
                .await
                .map_err(|e| format!("login {}: {e}", conn.email))?;
            *conn.client.write().await = client;
            conns.push(Arc::new(conn));
        }
        Ok(Self {
            conns,
            owner: Mutex::new(HashMap::new()),
            config,
            store,
        })
    }

    pub fn len(&self) -> usize {
        self.conns.len()
    }

    #[allow(dead_code)] // used by tests and future admin UI
    pub fn is_empty(&self) -> bool {
        self.conns.is_empty()
    }

    /// Bind a thread to an account: honour an existing/persisted binding, else
    /// assign the least-crowded healthy account.
    pub async fn route(&self, thread_id: &str) -> Arc<Connection> {
        {
            let owner = self.owner.lock().await;
            if let Some(&idx) = owner.get(thread_id) {
                return self.conns[idx].clone();
            }
        }

        // Persisted binding (survives restart) — unknown emails fall through.
        let bound = self
            .store
            .get_binding(thread_id)
            .ok()
            .flatten()
            .and_then(|email| self.conns.iter().position(|c| c.email == email));

        let idx = bound.unwrap_or_else(|| 0);
        let chosen = if bound.is_some() {
            idx
        } else {
            self.least_crowded().await
        };

        self.owner
            .lock()
            .await
            .insert(thread_id.to_string(), chosen);
        if bound.is_none() {
            let _ = self.store.bind(thread_id, &self.conns[chosen].email);
        }
        self.conns[chosen].clone()
    }

    async fn least_crowded(&self) -> usize {
        let counts = self.occupancy().await;
        let mut best = 0usize;
        let mut best_score = usize::MAX;
        for (i, c) in self.conns.iter().enumerate() {
            if !c.healthy().await {
                continue;
            }
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
            self.conns[idx]
                .penalize(self.config.login_cooldown)
                .await;
        }
        self.owner.lock().await.remove(thread_id);

        let healthy: Vec<usize> = {
            let mut v = Vec::new();
            for (i, c) in self.conns.iter().enumerate() {
                if Some(i) != current && c.healthy().await {
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

    /// Try to re-login an account (invalid/expired token recovery).
    pub async fn recover(&self, conn: &Arc<Connection>) -> bool {
        conn.reconnect().await.is_ok()
    }
}

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
