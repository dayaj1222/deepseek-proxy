# Hot reload plan — SIGHUP

Expands PLAN.md **T-15**. Written against the code as of `3f719ee`.

## Goal

Change `config.toml` (or the environment, or the account/key tables) and have a
running server pick it up **without dropping a single in-flight turn** and
without a restart. `kill -HUP <pid>` does it; `deepseek-rs reload` sends it for
you.

Non-goal: reloading `host`, `port` or `db_path`. Those are baked into the
listener and the open SQLite handle; a reload logs `restart required` and keeps
running on the old value.

## What is actually reloadable today

`Settings` (main.rs:62) is the whole live knobs surface. Every read is one of
these nine fields, at sixteen call sites:

| Field | Source | Reloadable |
|---|---|---|
| `model_name` | `MODEL_NAME` / `[deepseek] model` | yes |
| `thinking_default` | `THINKING_ENABLED` | yes |
| `search_enabled` | `SEARCH_ENABLED` | yes |
| `rate_limit_max_retries` | `RATE_LIMIT_MAX_RETRIES` | yes |
| `rate_limit_backoff_secs` | `RATE_LIMIT_BACKOFF_S` | yes |
| `tool_repair_max_retries` | `TOOL_REPAIR_MAX_RETRIES` | yes |
| `request_timeout_secs` | `REQUEST_TIMEOUT` | yes |
| `heartbeat_interval_secs` | `HEARTBEAT_INTERVAL` | yes |
| `image_max_bytes` | `IMAGE_MAX_BYTES` | yes |

Plus, outside `Settings`:

- **Pool knobs** (`request_delay`, `queue_limit`, `queue_timeout`,
  `account_fail_threshold`, `account_cooldown_max`) are copied **into each
  `Connection` at construction** (`Connection::new` takes `GateConfig`).
  Reloading them means updating live connections, not just a struct swap.
- **Accounts** come from `cli::load_accounts(&store)` → `connect_pool`, which
  performs a network login per account. Reload must not block on that.
- **API keys** are read per request via `store.verify_api_key`, so they are
  already live — `keys add`/`keys rm` needs no reload at all.

## Design

### 1. `Settings` behind one swappable handle

```rust
#[derive(Clone)]
struct AppState {
    settings: Arc<SettingsHandle>,   // was: Settings (by value)
    ...
}

struct SettingsHandle {
    current: std::sync::RwLock<Arc<Settings>>,
    loaded_at: Instant,
    generation: AtomicU64,
    /// Last reload failure, surfaced in /stats. `None` means healthy.
    last_error: std::sync::Mutex<Option<String>>,
}
```

Why this and not `arc-swap`: no new dependency, the critical section is a
pointer clone, and callers already hold `AppState` by clone so the `Arc`
travels into spawned turn tasks for free.

Call sites change from `state.settings.rate_limit_max_retries` to
`state.settings.get().rate_limit_max_retries`. Inside `stream_response` the
`LiveTurn` already carries `state`, so this is mechanical.

**One deliberate choice:** a turn snapshots its settings once at the top of
`stream_response` and uses that snapshot for its whole life. A turn that
started under a 300 s timeout must not have its deadline changed underneath it
mid-stream. New turns see new values; running turns finish on the old ones.

### 2. The reload itself

```rust
async fn reload(state: AppState) {
    // 1. Parse. Any error keeps the old config, logs, records in last_error.
    // 2. Non-reloadable keys: warn once, do not apply.
    // 3. Build the new Settings and swap it in.
    // 4. Push pool knobs into live connections (cheap, no I/O).
    // 5. Reconcile accounts in a spawned task (network logins).
}
```

Trigger: a dedicated `tokio::spawn` loop with `SignalKind::hangup()`, kept
separate from `shutdown_signal` so SIGHUP is never consumed by shutdown.

Reloads are **serialized** by a `tokio::Mutex<()>` so two SIGHUPs close
together cannot interleave an account diff.

### 3. Pool knobs on live connections

`Connection` keeps its `GateConfig` fields by value. Two options:

- **Rebuild the pool** when a pool knob changed. The new pool re-reads
  persisted thread→account bindings from SQLite (`Pool::route` already does),
  so no conversation is lost; live turns finish on the old pool. Cost: one
  login per account, milliseconds locally.
- **Fine-grained**: move the five tunables behind a per-connection
  `RwLock<GateConfig>` / atomics and re-read on each `acquire()`, carrying the
  existing `Arc<Connection>`s across so no re-login happens.

**Recommendation: start with the pool rebuild.** It is a tenth of the code, the
observable behaviour is identical, and ≤5 local clients do not notice the
logins. Switch to fine-grained only if logins become slow.

### 4. Account reconciliation (when the pool is rebuilt)

- **Added** email: login; on success add to the new pool. Failure logs and
  skips, exactly like boot.
- **Removed** email: absent from the new pool; its bound threads fall back to
  the least-crowded healthy account on their next turn.
- **Unchanged**: login re-runs (the cost of the rebuild option).

Logins happen **after** the settings swap, in a spawned task, so a slow
DeepSeek login never delays the reload of a timeout value.

### 5. CLI + observability

```
deepseek-rs reload        # SIGHUP via pidfile; "not running" if absent
```

Exit codes: `0` sent, `1` no pid / signal failed. It does **not** wait for the
server to confirm; confirmation is visible in `/stats` and the log.

`/stats` gains:

```json
"config": {
  "generation": 3,
  "loaded_at_secs_ago": 12,
  "last_error": null
}
```

Log lines:

- `info!("reloading config", reason = "SIGHUP")`
- `info!(generation, changed = ?names, "config reloaded")`
- `warn!(key = "port", "not reloadable; restart required")`
- `error!(error, "config reload failed; keeping the running config")`
- never log credentials, tokens, or full DSNs.

## Phased commits

| # | Change | Tests |
|---|---|---|
| 1 | `SettingsHandle`, mechanical call-site rewrite, no behaviour change | existing suite stays green; `settings_handle_swaps_atomically` |
| 2 | SIGHUP listener + `reload()` applying `Settings` only | `a_sighup_swaps_the_heartbeat_interval` |
| 3 | Non-reloadable key warning, parse-error retention, `/stats` `config` block | `an_invalid_config_keeps_the_old_one` |
| 4 | Account reconciliation via pool rebuild | `a_reload_adds_an_account_without_dropping_a_turn` |
| 5 | `deepseek-rs reload` + README | `reload_returns_1_without_a_pidfile` |

## Risks and how they are handled

| Risk | Mitigation |
|---|---|
| A turn's deadline changes mid-flight | Settings snapshotted per turn at the top of `stream_response` |
| Reload during shutdown | `reload()` checks `stats.shutting_down` first and returns |
| Two SIGHUPs interleave | `tokio::Mutex<()>` around the whole reload |
| Bad config bricks a running server | Parse first; on any error keep the old config and record `last_error` |
| A removed account strands a thread | Bindings live in SQLite; the new pool's `route` picks a healthy account |
| Secrets in logs | Never log `cfg`; log only changed key *names* |
| Reload blocks on a dead login | Logins run in a spawned task after the settings swap |
| SIGHUP swallowed by the shutdown handler | A dedicated `SignalKind::hangup()` loop |

## Open questions

1. Should a reload also re-read `DEEPSEEK_EMAIL_*` from the environment? Yes —
   `Config::load` already does; that is free.
2. Should `[limits] account_fail_threshold` changes reset open breakers?
   Recommendation: no — let existing breakers run out, so a reload cannot
   accidentally flood a flapping account.
3. Should `deepseek-rs reload` block until `/stats` shows the new generation?
   Recommendation: no by default, `--wait` if it turns out to matter.
