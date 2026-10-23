# CHANGELOG

A record of every landed change with the reasoning behind it. Newest first.
Shas refer to `main`; `git show <sha>` is the authoritative diff.

Roadmap status: [`../PLAN.md`](../PLAN.md). Decisions referenced as `ADR-nnnn`
live in [`adr/`](adr/).

---

## 2026-10-23 - Documentation pass

**`docs/` now explains the project, not just the API.** Added this changelog, an
ADR set covering the decisions already made in code, per-feature plans for
everything still on the roadmap, and a status table in `PLAN.md` so the next
contributor can tell done from planned without reading 43 commit messages.

*Files:* `docs/**`, `PLAN.md`

---

## Dialect

### `d1f7f49` fix(dialect): swallow up to two stray closing tags after a tool block

Models sometimes close a block two or three times. The extra closing tag leaked
into the prose the client sees. Up to `MAX_STRAY_CLOSERS` (2) closers
immediately after a completed block are now dropped by all three consumers:
`extract_tool_calls`, `scan`, and the streaming `Parser`. Anything beyond the
limit is left alone, so a literal tag quoted in prose still survives.

*Tests:* `swallows_two_extra_closing_tags`, `leaves_closing_tags_beyond_the_limit`
*Decision:* ADR-0011.

### `2ca04c1` feat(dialect): shorter repair prompt, carries the hard-format reminder

The repair turn sent three sentences of preamble plus a rules paragraph that
restated `TEMPLATE`. A long repair prompt competes with the wire format it is
trying to fix. The preamble is one sentence now, the redundant rules paragraph
is gone, a *think-first* line names the tags and the JSON body, and the repair
turn embeds `periodic_reminder()` so it carries the same hard-format warning as
a normal turn. The failure list and schema appendix are unchanged, so
`repair_schemas` still sends only the schemas that actually failed.

*Test:* the repair prompt contains `HARD FORMAT REQUIREMENT`.

### `a0b7c4e` test: reminder assertion matches the reminder text

Keeps the test honest after the prompt rewrite instead of asserting on a
substring that no longer exists.

---

## Reliability and operations

### `354ee32` feat(config): account reconciliation on reload, `deepseek-rs reload`

A reload now reconciles the account pool as well as the settings, and the CLI
can trigger it. The pool is **swapped**, not mutated (`AppState.pool` is
`RwLock<Arc<Pool>>`, read through `AppState::pool()`), so a turn already running
finishes on the pool it started on while new turns get the new one; no lock is
held across an await (ADR-0006).

A rebuild is paid for only when something changed: `PoolConfig` is `PartialEq`
and the pool records the configured emails *including ones that failed to log
in*, so a permanently dead account does not cause a rebuild on every SIGHUP.
Account changes are logged as added/removed. Reconciliation happens after the
settings swap, so a slow login never delays a timeout change. A pool that fails
to build leaves the running one in place - the same rule as an unparseable
config.

`deepseek-rs reload` sends SIGHUP and returns immediately; confirmation is
`config.generation` in `/stats` or the log. Exit 1 with no pidfile.
`accounts add` no longer says "restart to apply".

*Tests:* `a_reload_adds_an_account_without_restarting`, `reload_returns_1_without_a_pidfile`
*Not verified:* removing an account mid-turn (same code path, no HTTP test).

### `fb02494` feat(config): SIGHUP hot reload with retained-on-error semantics

The server re-reads `config.toml` on SIGHUP: no restart, no dropped turn. Three
behaviours matter more than the mechanism (ADR-0005):

1. **A turn snapshots settings once.** A reload mid-stream cannot move a
   deadline or change a retry budget under a turn that already started.
2. **A config that does not parse changes nothing.** The running config stays
   installed, the error is logged and exposed in `/stats`, and the generation
   does not advance - so "did my reload take?" is answerable without logs.
3. **Restart-only keys are reported, not pretended.** `host`, `port`, `db_path`
   and `image_max_bytes` cannot change while bound (the body limit is baked into
   the router layer), so a reload logs `not reloadable; restart required`.

SIGHUP has its own loop, separate from the shutdown signal; reloads serialize on
a tokio mutex so two signals cannot interleave; a reload arriving during
shutdown is ignored. `/stats` gains a `config` block: `generation`,
`loaded_at_secs_ago`, `last_error`.

*Harness:* every spawned proxy gets its own `config.toml` via `CONFIG_PATH`, so
tests never rewrite the repo's real config. `App` gained `pid()`, `sighup()`,
`write_config()`, and `stats_until()` to wait for an asynchronous reload instead
of sleeping.

*Tests:* `settings_handle_swaps_and_reports_generation`,
`a_failed_reload_keeps_the_running_config_and_records_the_error`,
`restart_only_keys_are_reported_not_applied`, `a_sighup_reloads_the_config`,
`an_invalid_config_keeps_the_running_one_and_reports_the_error`

### `2efede5` refactor(settings): read live knobs through a swappable handle

Groundwork for reload with no behaviour change: `Settings` moves behind
`Arc<SettingsHandle>` so it can be replaced at runtime. `get()` is one `Arc`
clone under a read lock, never held across an await - no `arc-swap` dependency
and no chance of a lock living through a stream. `backoff_delay` takes
`&Settings` rather than `&AppState` for the same reason. `build_router` reads
`image_max_bytes` once at build time, which is what makes it restart-only.

### `1efc162` feat(pool): circuit breaker for failing accounts

A flapping account was retried on every turn: a 5xx, a timeout or a dead login
only cooled the *login* path, so nothing stopped routing from handing it the
next thread. One bad account could burn the whole retry budget on every request
while healthy accounts sat idle.

Each account now counts consecutive failures. At `account_fail_threshold`
(default 3, 0 disables) the breaker opens and routing skips the account; the
wait doubles per extra failure from `login_cooldown_secs` up to
`account_cooldown_max_secs` (default 600). When the window elapses exactly one
probe is let through; success closes the breaker, another failure reopens it for
longer. A thread bound to an open account moves to the least-crowded healthy
one. `/stats` reports `breaker` and `retry_in_secs` per account.

Two constraints, both found the hard way (ADR-0007): the breaker state is
**not** behind the rate-gate mutex (the caller's `RateTicket` holds that for the
whole turn, so recording under it deadlocked), and a rate-limit cooldown is
**not** a breaker trip (`acquire()` still sleeps it out so the throttle retry
works).

*Tests:* `breaker_opens_at_the_threshold_and_recovers_after_the_window`,
`breaker_state_names_are_stable_for_stats`,
`a_failing_account_opens_its_breaker_and_the_thread_moves_on`
*Not verified:* against a real account; the flap is reproduced with the mock.

### `167b072` feat(threads): rewind a stale cursor before rebuilding the session

When DeepSeek answers `biz_code 26` (invalid message id) the saved parent is
gone. The only recovery used to be a full rebuild: a new remote session plus a
replay of the entire transcript - and on a long thread that replay is itself
what trips the upstream throttle, so a recoverable turn became a 429 loop.

The thread now keeps the last `REWIND_DEPTH` (3) committed cursors in memory.
On rejection the proxy pops one, persists it, prepends `LOST_REPLY_NOTE` and
resends the same delta on the same session: one extra turn, no replay, and the
remote context behind the cursor is kept. One rewind per turn; when there is
nothing left to step back to, the existing rebuild path runs unchanged
(ADR-0008).

*Tests:* `commit_keeps_a_bounded_history_of_cursors`,
`rewind_walks_back_one_commit_and_follows_the_persisted_row`,
`a_rebuilt_session_forgets_the_old_cursors`,
`a_rejected_parent_id_rewinds_to_the_previous_cursor`
*Not verified:* against a real account; the trigger is reproduced with a mock.

### `3f719ee` feat: non-streaming JSON, CORS, 501 stubs, bounded shutdown

Four gaps a real client hits, none of which showed up as an error.

1. **`stream: false` returned `text/event-stream`.** Every OpenAI SDK sends that
   by default and then fails to deserialize the body, so the most ordinary call
   looked broken. A turn now emits `sse::Chunk` and the caller picks the
   transport, so retries, tool repair, cursor rewinds and cursor persistence are
   the same code in both paths (ADR-0002).
2. **No CORS**, so a browser UI could not call the API at all (405 on
   preflight). The preflight is answered and every response carries
   `Access-Control-Allow-Origin: *` (ADR-0009).
3. **`/v1/embeddings`, legacy `/v1/completions`, `/v1/images/*`, `/v1/audio/*`**
   answered 404; they now answer 501 `not_supported` in the OpenAI envelope,
   which tells a capability probe the truth (ADR-0012).
4. **Graceful shutdown waited for in-flight SSE forever**, so one stalled
   stream blocked `stop` until SIGKILL. SIGTERM now flips `/readyz` to 503 at
   once, then in-flight turns get `shutdown_grace_secs` (default 20) before
   exit; `stop` waits grace + 5 s (ADR-0010).

Also: the test harness sets `DEEPSEEK_NO_DOTENV=1` so a spawned proxy cannot
pick up the developer's real accounts from `../deepseek/.env` - that made one
test see 4 accounts where it declared 2.

*Config:* `[limits] shutdown_grace_secs` / `SHUTDOWN_GRACE`
*Tests:* `a_non_streaming_request_still_gets_a_complete_answer`,
`a_non_streaming_tool_call_is_in_the_message`,
`cors_preflight_succeeds_and_responses_are_readable_by_browsers`,
`unsupported_endpoints_answer_501_not_404`

### `cc4cedd` feat(t-02): request id on every response, one log line per turn

`x-request-id` is minted when absent and echoed when the client supplies one, on
every response including 404/405 and errors. The id lives in a tracing span, so
every event logged while handling the request carries `req=` (ADR-0004).

The completed-turn line reports thread, account, status, ms and token counts in
one place; the failing-turn line reports status and code. The account is read
from the rate ticket that served the turn, because only that attempt knows which
account won the route after a failover.

### `4a57281` feat(t-01): OpenAI-shaped 401, 404 and 405 with a consistent error type

`authorize()` returned a bare `detail` object and `payload()` mapped every
non-400 to `server_error`. Official SDKs expect an `error` object, so a wrong
API key surfaced as a parse failure in the client. Every error now uses the
OpenAI envelope with a per-status `type` table, plus a JSON fallback for unknown
routes and a 405 handler (ADR-0003). `detail` is gone from the body - the one
intentional break from the Python port.

---

## Architecture and refactors (no behaviour change)

### `a4212d8` ... `e34919d` - the module split

Nine commits moved a 3.1k-line `main.rs` into modules with stated interfaces:
`dialect` (one parser grammar behind `scan` and the stream filter), `threads`
(one owner of cache, turn lock and persistence), `prompt` (one statement of the
ordering rules for a turn), `pool` (retry, cooldown and failover behind one
`complete()` seam), `turn` (`prepare()` and one `LiveTurn` struct replacing 16
positional arguments), `sse` (the upstream pump out of `main`). Dependencies
still point strictly downward: `proxy -> client -> protocol -> pow`.

### `7b17f2c`, `aeedec0` (T-19) - HTTP integration harness

`build_router` extracted from `main()`; the client's base URL became
overridable; a mock upstream replays `corpus/*.sse` and can script failures
(`NonSse`, `rate limit`, `login dead`, `context limit`, `stalled stream`).
Without this, every feature above would have been untestable.

---

## Earlier work (pre-split, summarized)

| Change | Why |
|---|---|
| Rust port of PoW, SSE router, auth client (`ea3e16c`) | The Python server's hot paths were the reason for the port |
| axum shell with stream + non-stream completions (`565f986`) | First usable surface |
| Tool dialect, SQLite persistence, auth, retry (`a32acb2`) | Durability and the tool-call contract |
| Incremental tool-call extraction across chunks (`3e5d649`) | A tool call arrives split over many SSE frames |
| Tool-result messages in continuation turns (`ec047a7`) | Without it a tool-using agent could not continue |
| Config TOML + CLI, multi-account pool, rate gate (`47511e6`) | Multiple logins, one shared budget |
| Context-limit recovery, cached login tokens (`19afaa4`) | A limit is recoverable; logins are expensive |
| Threads keyed by client session id, not message hash (`0017f43`) | Editing a message must not create a new conversation |
| Periodic tool-format reminder (`5435853`, `d690584`) | Long conversations drift away from the wire format |
| Compact colour-coded logs (`22eb83c`) | One line per event, readable in a terminal |
| Canonical Rust proxy; image intake degrades instead of failing (`b174462`) | A dropped image beats a wedged conversation |
| README rewritten, MIT license (`a7a632b`, `f9b8de5`) | Describe the server, not its lineage |
