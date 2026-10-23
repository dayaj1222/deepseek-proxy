# deepseek-rs

An OpenAI-compatible API server for DeepSeek. One binary, no runtime.

DeepSeek's chat has no public API. This server exposes the standard OpenAI
`/v1/chat/completions` surface, translates each request into what DeepSeek's
web client actually sends — login, proof-of-work, server-side sessions,
streamed tokens — and returns ordinary OpenAI JSON or SSE.

Point any OpenAI client at it and it works.

```bash
curl http://127.0.0.1:9174/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{"model":"deepseek-chat","messages":[{"role":"user","content":"hello"}]}'
```

## Features

- **OpenAI-compatible** — `/v1/chat/completions` and `/v1/models`, streaming
  and non-streaming, with `tool_calls` deltas and usage accounting.
- **Tool calling** — JSON-Schema validated arguments, and a bounded repair turn
  when the model emits a malformed call instead of surfacing the error.
- **Sessions** — conversations resume server-side, keyed by the client's session
  header, so only the new turn is uploaded.
- **Multiple accounts** — several DeepSeek accounts behind one endpoint, with
  per-account rate gating, health tracking, and failover.
- **Long threads** — if the backend reports a context limit, the session is
  rebuilt and the transcript replayed once, transparently.
- **Vision** — one image per turn, from a `data:` URL or a public HTTP URL.
- **Persistent state** — thread bindings, API-key hashes, and cached login
  tokens live in SQLite and survive restarts.
- **Single binary** — `cargo build --release`, done. No interpreter, no
  container, no virtualenv.

## Quick start

```bash
cargo build --release

export DEEPSEEK_EMAIL_0='you@example.com'
export DEEPSEEK_PASSWORD_0='your-password'

./target/release/deepseek-rs serve
```

Listens on `0.0.0.0:9174`; the base URL for clients is
`http://127.0.0.1:9174/v1`.

Run it detached instead — `start` returns only once the server answers
`/healthz`, so it is safe to chain on:

```bash
deepseek-rs start --bg
deepseek-rs status
deepseek-rs logs -f
```

## Configuration

Environment beats `config.toml` beats built-in defaults. Every key is optional.
`CONFIG_PATH` selects a different file; a missing file is not an error.

```toml
[proxy]
host = "0.0.0.0"
port = 9174

[storage]
# Relative paths resolve against this file's directory, not the process cwd.
db_path = "deepseek-rs.db"

[deepseek]
model    = "deepseek-chat"   # used when the client omits one
thinking = false             # default reasoning flag
search   = false             # web search (heavily throttled upstream)

[limits]
request_delay_secs = 2       # pause between requests on one account
queue_limit        = 64      # requests allowed to queue behind a busy account
queue_timeout_secs = 120     # how long one may wait for a slot
account_fail_threshold = 3   # consecutive failures that open a breaker; 0 off
account_cooldown_max_secs = 600  # cap for the exponential breaker cooldown
request_timeout_secs = 300   # per-turn wall clock
shutdown_grace_secs = 20     # in-flight turns may finish this long after SIGTERM
heartbeat_interval_secs = 10 # SSE keep-alive
rate_limit_max_retries = 5   # retries for a throttled turn
rate_limit_backoff_secs = 10.0
tool_repair_max_retries = 5  # 0 disables repair
image_max_bytes = 10485760   # 10 MiB, on bytes and base64 text
```

Frequently used environment overrides: `PROXY_HOST`, `PROXY_PORT`, `DB_PATH`,
`CONFIG_PATH`, `DEEPSEEK_RUNTIME_DIR`, `MODEL_NAME`, `THINKING_ENABLED`,
`SEARCH_ENABLED`, `REQUEST_DELAY`, `QUEUE_LIMIT`, `QUEUE_TIMEOUT`,
`REQUEST_TIMEOUT`, `HEARTBEAT_INTERVAL`, `IMAGE_MAX_BYTES`,
`RATE_LIMIT_MAX_RETRIES`, `RATE_LIMIT_BACKOFF_S`, `TOOL_REPAIR_MAX_RETRIES`,
`LOGIN_COOLDOWN`, `ACCOUNT_FAIL_THRESHOLD`, `ACCOUNT_COOLDOWN_MAX`,
`SHUTDOWN_GRACE`.

Credentials never go in `config.toml`.

## Accounts

Two sources, and the environment wins on an email collision.

```bash
# environment
export DEEPSEEK_EMAIL_0='a@example.com'
export DEEPSEEK_PASSWORD_0='...'
export DEEPSEEK_TOKEN_1='...'     # an existing bearer token also works

# or the database
deepseek-rs accounts add a@example.com 'password'
deepseek-rs accounts add b@example.com --token '...'
deepseek-rs accounts
deepseek-rs accounts rm a@example.com
```

Login tokens are cached and reused across restarts. A token the server rejects
is dropped and fetched again on the next attempt, so a stale cache heals
itself. An explicit `DEEPSEEK_TOKEN_<i>` always wins.

`deepseek-rs check` verifies login and proof-of-work for every account and
exits non-zero on failure.

## Client authentication

```bash
deepseek-rs keys add laptop sk-my-key
deepseek-rs keys
deepseek-rs keys rm laptop
```

Keys are stored as SHA-256 hashes; plaintext is never written. With **no** keys
registered, authentication is open — fine on a trusted loopback bind, worth
closing before exposing the port.

A rejected request returns `401` in the OpenAI error envelope —
`{"error":{"message","type":"invalid_request_error","param":null,"code"}}`
with `code` either `missing_api_key` or `invalid_api_key`, plus
`WWW-Authenticate: Bearer`. Official SDKs branch on that envelope, so this is the
one intentional break from the Python proxy, which answered `{"detail": ...}`.
Unknown paths return `404` and a known path with the wrong method returns `405`,
both in the same envelope (`not_found`, `method_not_allowed`).

## HTTP API

| Method | Path | Purpose |
|---|---|---|
| `POST` | `/v1/chat/completions` | Completions, streaming (`stream: true`, SSE) or not (one JSON `chat.completion`) |
| `GET` | `/v1/models` | Advertised model ids |
| `GET` | `/healthz` | Liveness |
| `GET` | `/readyz` | Readiness (`503` while draining after SIGTERM) |
| `GET` | `/stats` | Counters, per-account state, database counts |

`stream: false` (the OpenAI default) returns a single `chat.completion` object
with `choices[0].message` — including `reasoning_content` and `tool_calls` when
the turn produced them — and a `usage` block. `stream: true` returns SSE.

Endpoints this proxy does not implement — `/v1/embeddings`, the legacy
`/v1/completions`, `/v1/images/*`, `/v1/audio/*` — answer `501` with
`code: "not_supported"` rather than `404`, so a capability probe can tell
"this proxy won't do it" from "no such route".

CORS is fully open (`Access-Control-Allow-Origin: *`, preflight answered) so a
local browser UI can call the API directly. Authorization is the bearer key, not
the origin, so no allowlist is needed.

`/stats` is what `deepseek-rs stats` and `status` read:

```json
{"pid":0,"uptime_secs":0,"model":"deepseek-chat","thinking":false,"search":false,
 "requests":{"total":0,"ok":0,"errors":0,"auth_failed":0,"retries":0,
             "tool_calls":0,"images_dropped":0},
 "threads":{"in_memory":0},
 "db":{"threads":0,"bindings":0,"api_keys":0,"accounts":0,"cached_tokens":0},
 "accounts":[{"email":"...","healthy":true,"bound_threads":0,"waiting":0,"cooldown_ms":null,
              "breaker":"closed","retry_in_secs":null}],
 "config":{"generation":1,"loaded_at_secs_ago":0,"last_error":null}}
```

With the server down, `stats` falls back to database counts and says so on
stderr.

Every response, including errors, carries an `x-request-id` header. Send your
own to have it echoed back; otherwise the proxy mints one. The same id is on
every log line written while handling that request (`req=<id>`), so one id from
a bug report locates the whole turn.

## CLI

```
SERVER
  serve                       foreground (Ctrl-C stops)
  start [-b|--bg]             foreground by default; -b detaches, waits until healthy
  stop                        SIGTERM (SIGKILL after grace + 5s)
  reload                      SIGHUP: re-read config.toml and the account list
  restart [-b]                stop, then start
  status                      pid, url, uptime, account count
  health                      one-shot check (exit 1 when down)
  stats [--json]              counters, per-account state, db counts

LOGS
  logs [-n N] [-f] [--path P] last N lines (default 50); -f follows

ACCOUNTS
  accounts | accounts add <email> <password> | accounts add <email> --token <t> | accounts rm <email>

API KEYS
  keys | keys add <name> <key> | keys rm <name|id>

MISC
  check | url | version | help
```

Background mode keeps a pidfile and a log under `DEEPSEEK_RUNTIME_DIR`
(default `${XDG_STATE_HOME:-~/.local/state}/deepseek-rs`). `serve` claims the
pidfile too, so a second instance fails immediately instead of contending for
the database lock.

## Images

One image per turn, inline (`data:image/png;base64,...`) or as a public HTTP(S)
URL. It is uploaded once and referenced by file id, never re-sent on later
turns.

Attach several and the server sends the first, **drops the rest**, and tells the
model how many it lost so they can be re-attached one at a time. A hard
rejection would be worse: clients rebuild the same request on retry, so the turn
would never complete and the conversation would wedge. The dropped count is in
`/stats` as `requests.images_dropped`.

Remote URLs must resolve entirely to public addresses — loopback, link-local,
private, CGNAT, and reserved ranges are refused, and redirects are not followed.

## Behaviour worth knowing

- **Sessions are keyed by the client's session header** (`session_id`,
  `x-session-id`, `x-session-affinity`, `x-thread-id`, in that order), falling
  back to a hash of the first user message. The key maps to a server-side
  DeepSeek session, so the remote side holds the context.
- **Context limits are recovered, not reported.** A limit on a continuation
  triggers a fresh remote session and one transcript replay. Only before any
  content is sent, and only for existing threads.
- **Tool calls are repaired.** A malformed or schema-violating call gets a
  bounded correction turn instead of an error to the client.
- **Rate limits retry before the first byte**, so streaming clients can still be
  served; what cannot be retried fails with a typed error.
- **Shutdown is bounded.** On SIGTERM `/readyz` immediately reports `503`, then
  in-flight turns get `shutdown_grace_secs` to finish before the process exits.
- **Config is hot-reloadable.** `deepseek-rs reload` (or `kill -HUP <pid>`)
  re-reads `config.toml` and the account list without a restart and without
  dropping a turn: a turn keeps the settings it started with. A config that does
  not parse leaves the running one in place and is reported in
  `/stats.config.last_error`. `host`, `port`, `db_path` and `image_max_bytes`
  cannot change while bound; a reload logs `not reloadable; restart required`
  instead of pretending. Adding or removing an account rebuilds the pool in the
  background — thread bindings live in SQLite, so no conversation is stranded.
- **A flapping account is taken out of rotation.** After
  `account_fail_threshold` consecutive failures its breaker opens for
  `login_cooldown_secs * 2^k` (capped by `account_cooldown_max_secs`), its
  threads move to a healthy account, and one probe request tests it when the
  window elapses. `/stats` shows `breaker` and `retry_in_secs` per account.
- **Stale cursors are rewound, not replayed.** When the backend rejects the
  saved parent id, the proxy steps back one commit (keeping the last three) and
  resends the same delta, so the live session and its context survive. Only when
  there is nothing left to step back to does it rebuild the session from the
  transcript.

## Architecture

```
crates/pow       DeepSeekHashV1 + multithreaded nonce search
crates/protocol  SSE parsing + fragment routing + OpenAI chunk shaping
crates/client    login, session, PoW header, streaming, image upload
crates/proxy     axum server, tool dialect, SQLite storage, auth, retry
corpus/          captured SSE streams used as test fixtures
```

Dependencies point strictly downward: `proxy` → `client` → `protocol` → `pow`.
Nothing below `proxy` knows about HTTP serving.

## DeepSeekHashV1

Requests are gated behind a proof-of-work that is *almost* SHA3-256: the
Keccak-f[1600] permutation skips round 0 and runs rounds 1..=23. No off-the-shelf
SHA3 crate produces this digest, so the permutation lives in
`crates/pow/src/lib.rs`; round constants past `RC[0]` are standard Keccak.

## Development

```bash
cargo build --release
cargo test              # includes byte-for-byte corpus equality
cargo test -p proxy     # server-side behaviour only
```

The `corpus/` fixtures are raw byte streams captured from DeepSeek's completion
endpoint, paired with the routing output each should produce. They are what
keeps the stream parser honest.

### Documentation

| Where | What |
|---|---|
| [`docs/CHANGELOG.md`](docs/CHANGELOG.md) | every landed change, with the reason |
| [`docs/adr/`](docs/adr/) | why each design is the way it is |
| [`docs/features/`](docs/features/) | one-page plans for what is not built yet |
| [`PLAN.md`](PLAN.md) | task list, ground rules, and current status |
| [`GLOSSARY.md`](GLOSSARY.md) | thread, turn, delta, cursor, repair turn, account, surface |

### Logging

| | prod (default) | debug (`DEEPSEEK_LOG=debug`) |
|---|---|---|
| spans | hidden | shown |
| target / file / line | omitted | shown |
| thread ids | omitted | shown |
| ANSI | only when stdout is a TTY | only when stdout is a TTY |

`RUST_LOG` overrides the filter entirely (blank is ignored rather than meaning
"log nothing"); `NO_COLOR` or `DEEPSEEK_LOG_ANSI=1|0` forces colour off or on.

## License

MIT. See [LICENSE](LICENSE).
