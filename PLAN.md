# PLAN — feature roadmap for deepseek-rs

A work breakdown any agent (or human) can pick up cold. Each task is
self-contained: where to look, what to build, how to prove it works, what not to
touch. Use the vocabulary in `GLOSSARY.md` (thread, turn, delta, cursor, repair
turn, account, surface).

## 0. Ground rules (read first, apply to every task)

1. **One task = one branch = one commit series = one PR-sized change.** Branch
   name `feat/<task-id>-<slug>`. Never mix tasks.
2. **Before starting:** `git status`. The tree may hold someone's uncommitted
   work (at time of writing: `crates/proxy/src/main.rs`, `dialect.rs`,
   `GLOSSARY.md`). Do not stash, reset, or reformat files you don't own. If your
   task touches a dirty file, rebase on top of it and keep your diff minimal.
3. **Baseline must be green:** `cargo build --workspace && cargo test --workspace`.
   If it is not green before you start, stop and report; do not "fix" unrelated
   failures inside your task.
4. **Definition of done** for every task:
   - `cargo build --workspace` and `cargo test --workspace` pass.
   - `cargo clippy --workspace --all-targets` introduces **no new warnings**.
   - New behaviour has tests (unit, or integration against the mock upstream
     from T-19 once it exists). A bug fix gets a regression test that fails
     without the fix.
   - README updated if the surface (endpoint, CLI, config key, env var)
     changes. Config keys must be added in **three** places: `config.rs`
     (`*Section` struct + `Default` + `Config` + env override), `config.toml`
     example in README, and the "environment overrides" list.
   - No secret, token, password, or full prompt text in logs or in test
     fixtures.
5. **Compatibility:** never change existing request/response shapes in a way
   that breaks a working OpenAI client. New fields are additive. New config keys
   default to the old behaviour.
6. **Style:** match surrounding code (comment density, naming, error idiom).
   Dependencies point strictly downward: `proxy → client → protocol → pow`.
   Nothing below `proxy` may know about HTTP serving or SQLite.
7. **Don't hit the real DeepSeek in tests.** Real-account smoke checks are
   manual and opt-in (`deepseek-rs check`). Automated tests use fixtures in
   `corpus/` or the mock upstream (T-19).
8. **Schema migrations** (SQLite): append-only. Add `CREATE TABLE IF NOT EXISTS`
   / guarded `ALTER TABLE` inside `Store::migrate` (`store.rs`). Never edit or
   drop existing columns. A migration test must open a DB created by the
   previous schema.
9. **Report honestly:** in the PR/commit body list what was verified, what was
   not, and any behaviour you chose by assumption.

## 1. Repo map (verified against the code)

| Area | File | Notes |
|---|---|---|
| Server, handlers, routing, stream pump | `crates/proxy/src/main.rs` (~3.1k lines) | `chat_completions`, `stream_response`, `pump`, `validate_request`, `ProxyError`, `error_for`, `Settings`, `AppState`, `Stats`, router in `main()` |
| Account pool, gate, failover | `crates/proxy/src/pool.rs` | `Connection`, `RateTicket`, `Pool::route/failover/recover`, `cool_down` |
| SQLite store | `crates/proxy/src/store.rs` | tables: `threads`, `api_keys`, `thread_bindings`, `account_tokens`, `accounts` |
| Config | `crates/proxy/src/config.rs` | `FileConfig`, `LimitsSection`, `Config::load` |
| CLI | `crates/proxy/src/cli.rs` | `keys`, `accounts`, `stats`, `start/stop/logs` |
| Tool dialect, repair | `crates/proxy/src/dialect.rs` | `extract_tool_calls`, `build_repair_prompt`, `validation_reason` |
| Image intake | `crates/proxy/src/images.rs` | first image kept, rest dropped with note |
| Upstream client | `crates/client/src/lib.rs` | login, PoW header, `stream_completion`, `upload_image` |
| SSE parse/route | `crates/protocol/src/fragment.rs` | `FragmentRouter`, `Channel::{Reasoning,..}`; golden tests in `crates/protocol/tests/corpus.rs` |
| Fixtures | `corpus/*.sse` + `*.expected.json` | regenerate only via `tools/gen_expected.py` |

### Already implemented (do NOT rebuild; extend only where a task says so)

- OpenAI error envelope + `Retry-After` on 429 (`ProxyError`, `error_for`).
- Client-disconnect stops the upstream read (`pump`, log "client disconnected,
  stopping upstream read").
- Streaming `reasoning_content` deltas (`pump`, `Channel::Reasoning`).
- `keys add|rm|list` with SHA-256 hashes (`cli.rs::keys`, `store.rs`).
- `/stats`, `/healthz`, `/readyz`; axum graceful shutdown (`shutdown_signal`).
- Account failover on dead login (`Pool::failover`).

### Known gaps found while planning (feed into tasks below)

- `authorize()` returns `{"detail": ...}` for 401 — **not** the OpenAI envelope.
  Official SDKs expect `{"error": {...}}`. (→ T-01)
- `ChatRequest` silently ignores `max_tokens`, `max_completion_tokens`, `stop`,
  `n`, `seed`, `temperature`, `top_p`, `response_format`, `user`. (→ T-08..T-10)
- Non-streaming responses do not appear to return `reasoning_content`
  (`route_all` is only used by the repair path). Verify first. (→ T-11)
- Graceful shutdown waits for in-flight SSE with no deadline, so a hung stream
  blocks `stop` until SIGKILL at 15 s. (→ T-07)
- No tests drive the HTTP surface end to end; no mock upstream. (→ T-19)

## 2. Dependency graph and order

```
Phase A  foundation      T-19 mock upstream  ──┐   T-00 split main.rs (optional)
Phase B  correctness     T-01 T-02 T-03        │   (need T-19 for integration tests)
Phase C  API surface     T-08 T-09 T-10 T-11 T-12 T-13 T-14
Phase D  operations      T-04 T-05 T-06 T-07 T-15 T-16
Phase E  packaging/docs  T-17 T-18
```

Hard dependencies: T-19 before any task that needs an HTTP-level test
(T-01..T-16). T-00 should land **before** parallel work starts, or be skipped
entirely — do not run it concurrently with other `main.rs` tasks. T-05 depends
on T-04. T-12 (n>1) depends on T-08. T-14 (/v1/responses) depends on T-08..T-11.

Suggested order for a single agent: **T-19, T-01, T-02, T-03, T-11, T-08, T-09,
T-10, T-04, T-05, T-07, T-06, T-13, T-12, T-14, T-15, T-16, T-17, T-18**, with
T-00 first only if the team wants smaller files.

Effort key: S ≤ 0.5 day, M ≈ 1–2 days, L ≈ 3+ days.

---

## Phase A — Foundation

### T-19 · Mock upstream + HTTP integration harness  [M] — do first

**Why:** every other task needs a way to exercise the real axum router without a
DeepSeek account.

**Where:** new `crates/proxy/tests/` (integration tests) and a small
`crates/proxy/src/lib.rs` re-export *only if needed* to build the router from
tests. Prefer extracting `fn build_router(state: AppState) -> Router` from
`main()` in `main.rs` (pure move, no behaviour change).

**Steps**
1. Extract `build_router`. Keep `main()` calling it. Commit separately.
2. `client::Client` hard-codes the DeepSeek base URL. Add an overridable base
   (field + `Client::with_base_url`, defaulting to the production URL; env
   `DEEPSEEK_BASE_URL` for tests only, documented as unsupported). Cover
   login, `create_chat_session`, `pow_challenge`, `stream_completion`,
   `upload_image`.
3. Write a tiny axum mock (in `tests/support/mock_upstream.rs`) that serves
   those endpoints and replays `corpus/*.sse` bytes. Controls via a shared
   `MockScript` (queue of behaviours): success, `rate limit`, `login dead`,
   `context limit`, `stalled stream`, `malformed tool call then valid`.
4. PoW: the mock must hand out a trivially solvable challenge (low difficulty)
   so `crates/pow` runs for real but fast.
5. Test helper `spawn_app(config_overrides) -> (base_url, MockHandle)` using an
   in-memory store (`Store::open_memory`).
6. First tests: non-stream hello, stream hello, 429 retry then success,
   login-dead failover with two mock accounts, malformed tool call → repair.

**Acceptance:** `cargo test -p proxy --test http` runs offline in < 30 s and
covers the six scenarios above. No change to production behaviour (diff in
non-test code limited to `build_router` extraction and the base-URL override).

**Don't:** change `corpus/` fixtures or `protocol` parsing.

### T-00 · Split `main.rs` into modules  [M] — optional, do only if serialized

Move, without behaviour change: `errors.rs` (`ProxyError`, `CompletionError`,
`error_for`), `auth.rs` (`authorize`), `routes.rs` (handlers, router),
`stream.rs` (`pump`, `stream_response`, SSE helpers), `prompt.rs`
(`build_prompts`, transcript/delta helpers, `thread_key`). Acceptance:
`cargo test --workspace` count of passing tests unchanged; `git diff -M`
shows moves only. Land alone, with no other branch open against `main.rs`.

---

## Phase B — Correctness

### T-01 · OpenAI-shaped 401 and consistent error `type`  [S]

**Problem:** `authorize()` returns `{"detail": ...}`; `ProxyError::payload`
maps every non-400 to `"server_error"`.

**Steps**
1. 401 → `{"error":{"message","type":"invalid_request_error","param":null,"code":"invalid_api_key"|"missing_api_key"}}`, header `WWW-Authenticate: Bearer`.
2. `payload()` type table: 400→`invalid_request_error`, 401→`invalid_request_error`, 404→`invalid_request_error`, 429→`rate_limit_error`, 5xx→`server_error`.
3. Add 404 JSON fallback for unknown routes and 405 handler using the same envelope (axum `fallback`).
4. Keep `detail` out of the body (document the change in the commit message —
   it is the one intentional break from the Python port).

**Tests:** table-driven test over every `ProxyError` constructor; HTTP test for
missing/invalid key; unknown path returns JSON, not empty body.

### T-02 · Request-id and structured request logging  [S]

Add `x-request-id` (accept inbound, else generate via `uuid_like()`), echo it in
every response header and error body is **not** modified. Put it in a tracing
span so `log.rs` compact format shows a short id. Acceptance: header present on
success, stream, and error responses; one log line per completed turn with
`req`, `thread`, `account`, `status`, `ms`, `prompt_tokens`, `completion_tokens`.

### T-03 · Cooldown + circuit breaker for failing accounts  [M]

**Where:** `pool.rs` (`Connection`, `healthy`, `cool_down`, `failover`).

**Design:** per-connection consecutive-failure counter (login-dead, 5xx,
timeouts; *not* client 4xx). On N failures (`[limits] account_fail_threshold`,
default 3) open the breaker: skip in `route()` for `base * 2^k` seconds, capped
(`account_cooldown_max_secs`, default 600). Half-open: allow one probe request;
success closes it. Expose state in `/stats` per account
(`breaker: "closed"|"open"|"half_open"`, `retry_in_secs`).

**Tests:** unit tests on the state machine with a fake clock (inject
`Instant` via a small trait or `tokio::time::pause`); integration test with two
mock accounts where one flaps. **Don't** change thread→account binding
semantics beyond skipping an open account.

---

## Phase C — API surface

### T-08 · Honour `max_tokens` / `max_completion_tokens` / `stop`  [M]

**Constraint:** upstream cannot be told these, so enforce proxy-side in the
`pump`/non-stream path.

**Steps**
1. Add fields to `ChatRequest` (`max_tokens`, `max_completion_tokens`, `stop`
   as string or array ≤ 4, validated in `validate_request` with
   `param` set on error).
2. Stop-sequence matcher that works across chunk boundaries (hold back
   `max(len(stop))-1` trailing bytes, like `partial_tag_suffix` in `dialect.rs`).
   Applies to prose only, never inside a tool-call block.
3. Token cap using the existing `estimate_tokens` (tiktoken). On hit: stop
   emitting, `finish_reason:"length"`; on stop match: `finish_reason:"stop"`.
   Cancel the upstream read the same way the disconnect path does.
4. Interaction with tool calls: if a tool call has begun, let the block finish
   (never emit a truncated call).
5. The turn cursor must still advance (see GLOSSARY *cursor*) — truncating the
   client view must not leave a stale `parent_message_id`.

**Tests:** stop split across two SSE chunks; stop inside reasoning ignored;
cap hit mid-prose; cap hit during tool block; non-stream equivalents; cursor
persisted after early stop.

### T-09 · Accept-and-ignore sampling params with visibility  [S]

`temperature`, `top_p`, `presence_penalty`, `frequency_penalty`, `seed`,
`user`, `logit_bias`, `logprobs`: accept without error (many SDKs always send
them), log once per process at `debug`, and surface `x-deepseek-rs-ignored:
temperature,top_p` response header. `logprobs:true` → 400 `unsupported_parameter`
(cannot be faked). Update README "Behaviour worth knowing".

### T-10 · `response_format` (`json_object`, `json_schema`)  [M]

**Reuse:** `jsonschema` validation and the repair-turn machinery
(`dialect.rs` `build_repair_prompt`/`Recovery` in `main.rs`).

**Steps:** inject a system instruction demanding JSON only (and the schema for
`json_schema`); on completion, parse+validate; on failure run up to
`tool_repair_max_retries` repair turns with a JSON-specific prompt; on final
failure return 502 `invalid_model_output` (never return invalid JSON as
success). Streaming: buffer content, emit once validated (document that
`response_format` disables incremental streaming; `stream:true` still returns
valid SSE, one content chunk). `strict:true` honoured the same way.

**Tests:** valid first time; invalid then repaired; never valid → 502; schema
with external `$ref` rejected (reuse `has_external_ref`); combined with `tools`
→ 400 (unsupported combination, documented).

### T-11 · `reasoning_content` in non-streaming responses  [S]

**First verify:** curl a non-stream request with thinking on and inspect
`choices[0].message`. If `reasoning_content` is missing, build it from the same
router output used in streaming (`Channel::Reasoning`) and add it to the
message; omit the key when thinking is off. Also expose reasoning token count
in `usage.completion_tokens_details.reasoning_tokens`. Test with
`corpus/think.sse` through the mock.

### T-12 · `n > 1`  [M] (depends T-08)

Run `n` sequential turns on **sibling sessions** (new remote session per
choice, so they don't fork the primary thread's cursor); only choice 0 updates
the thread binding/cursor. Cap `n` at `[limits] max_n` (default 4); streaming
with `n>1` → 400. `usage` sums all choices. Document the cost (n× rate-limit
budget) and that choices are not independent samples of one prompt state if the
prompt had images (upload once, reference by file id).

### T-13 · Multiple images per turn  [M]

**Where:** `images.rs` (`load`), `client::upload_image`, `build_prompts`.
Currently first kept, rest dropped with a note.

**Steps:** probe upstream with a mock first — if DeepSeek accepts >1 file id
per message, send them all (cap `[limits] max_images_per_turn`, default 4,
total byte cap). If not, keep current behaviour and implement the *fallback*
only: stitch ≤4 images into a grid with the `image` crate (feature-gated,
off by default). Preserve the SSRF rules in `fetch_remote` / `is_global*`
exactly — no loosening. Update the `images_dropped` stat semantics and README.
**Acceptance:** test with 3 data-URL images; remote-URL SSRF tests still pass.

### T-14 · `/v1/responses` endpoint  [L] (depends T-08..T-11)

Map the Responses API (`input` string|items, `instructions`, `tools`,
`previous_response_id`, `store`, `stream` with `response.output_text.delta`
events, `response.completed`) onto the existing core by translating to a
`ChatRequest`. Store `response_id → thread_key` in SQLite (new table
`responses`, migration per rule 8) so `previous_response_id` resumes the
thread. Do not duplicate prompt-building logic: translate, then call the shared
turn function (extract it from `chat_completions` if not already callable).
Start with text + function tools; reject unsupported item types with 400
`unsupported_input_type`. **Tests:** non-stream, stream event order, resume via
`previous_response_id`, tool round-trip.

### T-20 · `/v1/embeddings` honest stub  [S]

Return 501 with the OpenAI envelope, `code:"not_supported"`, message pointing
to the README. (A fake embedding would silently poison a RAG index; refuse.)
Same for `/v1/audio/*`, `/v1/images/*`, `/v1/completions` (legacy) — one
handler. Listed in README under "Not supported".

---

## Phase D — Operations

### T-04 · API-key metadata: label, expiry, per-key limits  [M]

**Where:** `store.rs` (`api_keys` table, `insert_api_key`, `verify_api_key`),
`cli.rs::keys`, `authorize`.

**Schema (additive):** `expires_at INTEGER NULL`, `models TEXT NULL` (JSON
allowlist), `rpm INTEGER NULL`, `disabled INTEGER DEFAULT 0`, `last_used_at`,
`created_at`.

**CLI:** `keys add <name> <key> [--expires 30d] [--models a,b] [--rpm N]`,
`keys disable|enable <name>`, `keys` shows expiry/last-used (never the key).
`verify_api_key` returns a `KeyId` (not bool) so handlers can attribute usage.
Keep "no keys registered = open" behaviour. `last_used_at` updates are
debounced (≤ 1 write/min/key) to avoid a write per request.

**Tests:** migration from old schema; expired → 401 `expired_api_key`;
disallowed model → 403 `model_not_allowed`; hash still not reversible.

### T-05 · Usage accounting + per-key rate limit  [M] (depends T-04)

New table `usage(key_id, day, requests, prompt_tokens, completion_tokens,
errors)` upserted in batches from an in-memory aggregator flushed every 30 s and
on shutdown. In-memory token-bucket for `rpm`, 429 `rate_limit_exceeded` with
`Retry-After`. CLI: `deepseek-rs usage [--key NAME] [--days N] [--json]`. Token
numbers are the proxy's estimates (say so in README). **Tests:** bucket
refill with paused time; flush on shutdown; usage survives restart.

### T-06 · Prometheus `/metrics`  [M]

Add `GET /metrics` (text format) behind the same auth as `/stats` (or a separate
`[proxy] metrics_token`). Metrics: `dsrs_requests_total{status,stream}`,
`dsrs_request_seconds` histogram, `dsrs_queue_waiting{account}`,
`dsrs_account_healthy{account}`, `dsrs_breaker_state{account}`,
`dsrs_pow_seconds` histogram, `dsrs_repair_turns_total`,
`dsrs_failovers_total`, `dsrs_tokens_total{kind}`, `dsrs_images_dropped_total`.
Implement with plain atomics/`Mutex<Histogram>` and hand-rolled text output to
avoid a heavy dependency, unless the `prometheus`-style crate adds < 5 deps.
Never put emails in labels — use a stable short hash (`acct-3f2a`). **Tests:**
scrape after a mock request, assert names and monotonic counters.

### T-07 · Shutdown drain with deadline  [S]

On SIGTERM/Ctrl-C: stop accepting, mark `/readyz` 503 immediately, give
in-flight turns `[limits] shutdown_grace_secs` (default 20), then cancel
remaining streams (send an SSE error + `[DONE]`), persist cursors, flush T-05
usage, exit 0. Align the CLI `stop` SIGKILL timeout to `grace + 5`. **Tests:**
mock stalled stream + shutdown → exits within grace; cursors persisted.

### T-15 · Hot reload (SIGHUP)  [M]

`Settings` is cloned into `AppState`; make the reloadable subset
(`rate_limit_*`, `tool_repair_max_retries`, `request_timeout_secs`,
`heartbeat_interval_secs`, default model/thinking/search, limits) live behind
`Arc<ArcSwap<Settings>>` (or `RwLock`). SIGHUP re-reads `config.toml` + DB
accounts + keys: add new accounts to the pool without dropping live ones, mark
removed accounts draining. Non-reloadable keys (host, port, db_path) log a
warning "restart required". `deepseek-rs reload` sends SIGHUP via the pidfile.
**Tests:** add account at runtime via mock; change heartbeat; invalid config
keeps the old config and logs the error.

### T-16 · Debug capture and replay  [M]

Opt-in `[debug] capture_dir` (off by default, never on in `serve` unless set).
Write one JSON file per turn: redacted request (no auth header, no tokens,
messages truncated to N chars unless `capture_full=true`), raw upstream SSE
bytes, timings, decisions (retry count, repair, failover). `deepseek-rs replay
<file>` feeds captured SSE through `protocol::FragmentRouter` and prints the
routed output — enough to turn a field bug into a new `corpus/` fixture.
File permissions 0600, size-capped ring (`capture_max_files`). Add an
**unknown-event counter** in `protocol` surfaced in `/stats` so upstream drift is
visible. **Tests:** redaction (assert no bearer/password strings in output),
ring eviction.

---

## Phase E — Packaging and docs

### T-17 · Deployment artifacts  [S]

`contrib/deepseek-rs.service` (systemd user unit, `Restart=on-failure`,
hardening flags: `NoNewPrivileges`, `ProtectSystem=strict`,
`ReadWritePaths=` state dir), multi-stage `Dockerfile` (distroless/static or
debian-slim, non-root, `HEALTHCHECK` on `/healthz`), `PKGBUILD` for AUR,
GitHub Actions workflow (`fmt`, `clippy -D warnings` once baseline is clean,
`test`, release build artifact). README "Deployment" section. Validate:
`docker build .` succeeds, `systemd-analyze verify` on the unit.

### T-18 · Optional TLS and exposure guard  [M]

`[proxy] tls_cert`, `tls_key` (rustls via `axum-server` or `tokio-rustls`).
Independently: when bound to a non-loopback address **and** no API keys exist,
log a loud startup warning and (new default) refuse to start unless
`[proxy] allow_open = true` / `ALLOW_OPEN=1`. This is a behaviour change for
existing deployments — call it out in the README and release notes. **Tests:**
config matrix for the guard; TLS handshake test with a self-signed cert
generated in-test.

---

## 3. Task template (copy into the PR description)

```
Task: T-xx <title>
Branch: feat/t-xx-<slug>
Baseline: <commit sha>, tests green: yes/no
Changed: <files>
Added config/env/CLI/endpoint: <list or none>
Migrations: <none | table/column list + migration test name>
Tests added: <names>
Verified manually: <what, against mock or real account>
Not verified / assumptions: <list>
Docs updated: README sections
```

## Status as of 2026-10-23

Every landed change and its reasoning is in [`docs/CHANGELOG.md`](docs/CHANGELOG.md).
Every decision is in [`docs/adr/`](docs/adr/). Every planned feature has a
one-page plan in [`docs/features/`](docs/features/).

| Task | Title | Status |
|---|---|---|
| T-19 | Mock upstream + HTTP integration harness | **done** (`aeedec0`, `7b17f2c`) |
| T-00 | Split `main.rs` into modules | **done** (`a4212d8`…`e34919d`), wider than the plan |
| T-01 | OpenAI-shaped 401/404/405 | **done** (`4a57281`) |
| T-02 | Request id + one log line per turn | **done** (`cc4cedd`) |
| T-03 | Cooldown + circuit breaker | **done** (`1efc162`) |
| — | Cursor rewind before rebuild (not in the original plan) | **done** (`167b072`) |
| T-07 | Shutdown drain with deadline | **done** (`3f719ee`) |
| T-11 | `reasoning_content` in non-streaming replies | **partial** — `message.reasoning_content` ships (`main.rs:2107`, via the shared `Chunk` fold); `usage.completion_tokens_details.reasoning_tokens` is still missing |
| T-15 | Hot reload (SIGHUP) | **done** (`2efede5`, `fb02494`, `354ee32`) |
| T-20 | 501 stubs for unsupported endpoints | **done** (`3f719ee`) |
| — | Non-streaming JSON, CORS (not in the original plan) | **done** (`3f719ee`) |
| — | Stray closing tags after a tool block | **done** (`d1f7f49`) |
| T-04 | API-key metadata | planned — [plan](docs/features/T-04-api-key-metadata.md) |
| T-05 | Usage accounting + per-key rate limit | planned — [plan](docs/features/T-05-usage-accounting.md) |
| T-06 | Prometheus `/metrics` | planned — [plan](docs/features/T-06-prometheus-metrics.md) |
| T-08 | `max_tokens` / `stop` | planned — [plan](docs/features/T-08-token-limits-and-stop.md) |
| T-09 | Accept-and-ignore sampling params | planned — [plan](docs/features/T-09-sampling-params.md) |
| T-10 | `response_format` | planned — [plan](docs/features/T-10-response-format.md) |
| T-12 | `n > 1` | planned — [plan](docs/features/T-12-n-choices.md) |
| T-13 | Multiple images per turn | planned — [plan](docs/features/T-13-multiple-images.md) |
| T-14 | `/v1/responses` | planned — [plan](docs/features/T-14-responses-api.md) |
| T-16 | Debug capture and replay | planned — [plan](docs/features/T-16-capture-and-replay.md) |
| T-17 | Deployment artifacts | planned — [plan](docs/features/T-17-deployment-artifacts.md) |
| T-18 | Optional TLS and exposure guard | planned — [plan](docs/features/T-18-tls-and-exposure-guard.md) |

T-07 and T-20 were closed by work the plan did not anticipate: bounded shutdown
and the 501 stubs both shipped in `3f719ee` alongside non-streaming JSON. T-11 is
half done - the reply now carries `reasoning_content` because a turn emits
`Chunk`s and the caller chooses the transport (ADR-0002), so the streaming and
non-streaming paths share one fold; the `reasoning_tokens` count is still
missing and has its own page in `docs/features/`. Cursor rewind, CORS and
non-streaming JSON were not in the original plan at all; they were gaps found by
using the server.

**Next, if picking up cold:** T-04 → T-05 (usage needs key ids), then T-08
(the most-requested API gap), then T-09 and T-10. T-19 is done, so every task
below can be tested at the HTTP level from day one.

## 4. Risk register (check before merging anything touching these)

| Risk | Where | Guard |
|---|---|---|
| Stale cursor forks the conversation | `persist_cursor`, early-stop paths (T-08, T-12, T-07) | Test asserts `parent_message_id` advanced after every terminated turn |
| Turn lock held across new await points | `turn_guard` | No new locks acquired while the turn guard is held; reviewers check lock ordering |
| SSRF regression | `images.rs` (T-13) | Keep the `is_global*` and no-redirect tests green; add, never remove, cases |
| Token/PII in logs or captures | T-16, T-02 | Redaction test; grep the diff for `Authorization`, `password`, `token` |
| Breaking OpenAI clients | T-01, T-08..T-12 | Run `corpus`-based HTTP tests plus one official-SDK smoke (manual) |
| SQLite lock contention | T-05 writes | Batch + debounce; never write on the request hot path |
| Upstream protocol drift | `protocol`, `client` | T-16 unknown-event counter; fixtures regenerated only via `tools/gen_expected.py` |
