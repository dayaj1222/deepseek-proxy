# deepseek-rs

An OpenAI-compatible HTTP proxy for DeepSeek's private chat API, written in Rust.

DeepSeek's web chat has no public API. This project speaks the OpenAI
`/v1/chat/completions` protocol to your tools and translates it into the
private endpoints the web client uses — including login, proof-of-work,
server-side thread resume, and streamed token reassembly — then hands the
result back as standard OpenAI JSON or SSE.

It ships as a single binary. No runtime, no Python environment, no container.

> This is the canonical implementation. The Python proxy it was ported from is
> preserved unchanged on the `python-archive` branch (tag `python-final`) for
> reference and for the captured SSE corpus. It is not maintained for running.

---

## Contents

- [Why](#why)
- [Features](#features)
- [Quick start](#quick-start)
- [Configuration](#configuration)
- [Accounts](#accounts)
- [Client authentication](#client-authentication)
- [HTTP API](#http-api)
- [CLI](#cli)
- [Images](#images)
- [Behaviour worth knowing](#behaviour-worth-knowing)
- [Architecture](#architecture)
- [DeepSeekHashV1](#deepseekhashv1)
- [Development](#development)
- [License](#license)

---

## Why

The Python proxy worked, but it carried an interpreter, a virtualenv, and an
async stack in front of what is fundamentally a protocol translator. This port
keeps the exact wire behaviour — the same prompts, the same role framing, the
same tool dialect — while collapsing the runtime to one statically linked
binary that starts in milliseconds and holds a small, predictable footprint.

Byte-level fidelity was the acceptance bar, not "works well enough": the SSE
fragment router is tested against real captured streams and asserted
byte-identical to the Python output.

## Features

- **OpenAI-compatible surface** — `/v1/chat/completions`, `/v1/models`, plus
  `/healthz`, `/readyz`, and `/stats`.
- **Streaming and non-streaming** — SSE chunks shaped exactly like OpenAI's,
  including `tool_calls` deltas and usage accounting.
- **Tool calling** — JSON-Schema validated arguments, configurable dialect,
  and automatic repair turns when the model emits a malformed call.
- **Thread resume** — conversations are resumed server-side via DeepSeek's own
  session ids, keyed by the client's session header rather than by message
  hash, so a client keeps one remote conversation.
- **Multi-account pool** — several DeepSeek accounts behind one endpoint, with
  per-account rate gating, health tracking, and failover.
- **Context-limit recovery** — an `input_exceeds_limit` reply triggers a fresh
  remote session and a single transcript replay, transparently.
- **Vision input** — one image per turn, from a `data:` URL or a public HTTP URL.
- **SQLite state** — thread bindings, API-key hashes, and cached login tokens
  survive restarts.

## Quick start

```bash
cargo build --release

# credentials, either inline or from the environment
export DEEPSEEK_EMAIL_0='you@example.com'
export DEEPSEEK_PASSWORD_0='your-password'

./target/release/deepseek-rs serve
```

The server listens on `0.0.0.0:9174` by default and exposes an
OpenAI-compatible base URL of `http://127.0.0.1:9174/v1`.

```bash
curl http://127.0.0.1:9174/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{"model":"deepseek-chat","messages":[{"role":"user","content":"hello"}]}'
```

Run it detached with a health gate instead:

```bash
deepseek-rs start --bg     # returns once /healthz answers
deepseek-rs status
deepseek-rs logs -f
```

## Configuration

Precedence is **environment > `config.toml` > built-in defaults**. Every key is
optional; a partial file is valid. `CONFIG_PATH` selects a different file, and
a missing file is not an error.

```toml
[proxy]
host = "0.0.0.0"
port = 9174

[storage]
# Relative paths are anchored to this file's directory, never the process cwd.
db_path = "deepseek-rs.db"

[deepseek]
model    = "deepseek-chat"   # model id returned when the client omits one
thinking = false             # default reasoning flag
search   = false             # web-search flag (throttled hard upstream)

[limits]
request_delay_secs = 2       # pause between requests on one account
queue_limit        = 64      # queued requests tolerated behind a busy account
queue_timeout_secs = 120     # how long a request may wait for a slot
request_timeout_secs = 300   # per-turn wall clock
heartbeat_interval_secs = 10 # SSE keep-alive interval
rate_limit_max_retries = 5   # retries for a throttled turn
rate_limit_backoff_secs = 10.0
tool_repair_max_retries = 5  # 0 disables tool-call repair
image_max_bytes = 10485760   # 10 MiB, applied to bytes and base64 text
```

The commonly used environment overrides are `PROXY_HOST`, `PROXY_PORT`,
`DB_PATH`, `CONFIG_PATH`, `DEEPSEEK_RUNTIME_DIR`, `MODEL_NAME`,
`THINKING_ENABLED`, `SEARCH_ENABLED`, `REQUEST_DELAY`, `QUEUE_LIMIT`,
`QUEUE_TIMEOUT`, `REQUEST_TIMEOUT`, `HEARTBEAT_INTERVAL`,
`IMAGE_MAX_BYTES`, `RATE_LIMIT_MAX_RETRIES`, `RATE_LIMIT_BACKOFF_S`, and
`TOOL_REPAIR_MAX_RETRIES`.

Credentials are never stored in `config.toml`.

## Accounts

Accounts come from two places, and environment entries win on an email
collision:

```bash
# environment
export DEEPSEEK_EMAIL_0='a@example.com'
export DEEPSEEK_PASSWORD_0='...'
export DEEPSEEK_TOKEN_1='...'     # a pre-existing bearer token also works

# or SQLite
deepseek-rs accounts add a@example.com 'password'
deepseek-rs accounts add b@example.com --token '...'
deepseek-rs accounts
deepseek-rs accounts rm a@example.com
```

Login tokens are cached in SQLite and reused across restarts. A token the
server rejects is dropped and re-fetched on the next recovery, so a stale cache
self-heals without intervention. An explicitly supplied `DEEPSEEK_TOKEN_<i>`
always wins, since it is deliberate.

`deepseek-rs check` verifies login and proof-of-work for every configured
account and exits non-zero on failure — useful before restarting a fleet.

## Client authentication

Register keys to require a bearer token from clients:

```bash
deepseek-rs keys add laptop sk-my-key
deepseek-rs keys
deepseek-rs keys rm laptop
```

Keys are stored as SHA-256 hashes; the plaintext is never persisted. If **no**
keys are registered, authentication is open — convenient on a trusted loopback
bind, and worth closing before exposing the port.

## HTTP API

| Method | Path | Purpose |
|---|---|---|
| `POST` | `/v1/chat/completions` | Chat completions, streaming or not |
| `GET` | `/v1/models` | Advertised model ids |
| `GET` | `/healthz` | Liveness |
| `GET` | `/readyz` | Readiness (login is lazy, so up means ready) |
| `GET` | `/stats` | Counters, per-account state, database counts |

`/stats` backs both `deepseek-rs stats` and `deepseek-rs status`:

```json
{"pid":0,"uptime_secs":0,"model":"deepseek-chat","thinking":false,"search":false,
 "requests":{"total":0,"ok":0,"errors":0,"auth_failed":0,"retries":0,
             "tool_calls":0,"images_dropped":0},
 "threads":{"in_memory":0},
 "db":{"threads":0,"bindings":0,"api_keys":0,"accounts":0,"cached_tokens":0},
 "accounts":[{"email":"...","healthy":true,"bound_threads":0,"waiting":0,"cooldown_ms":null}]}
```

With the server down, `stats` falls back to database counts and says so on
stderr.

## CLI

One binary, `deepseek-rs`. `serve` is the only long-running command.

```
SERVER
  serve                       foreground (Ctrl-C stops)
  start [-b|--bg]             foreground by default; -b detaches and waits until healthy
  stop                        SIGTERM (SIGKILL after 15s)
  restart [-b]                stop, then start
  status                      pid, url, uptime, account count
  health                      one-shot health check (exit 1 when down)
  stats [--json]              live counters, per-account state, db counts

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
pidfile too, so a second instance fails immediately rather than contending for
the SQLite lock.

## Images

A turn may carry **one** image, either inline (`data:image/png;base64,...`) or
as a public HTTP(S) URL. The image is uploaded once per turn and referenced by
file id, so it is never re-sent on later turns.

If a client attaches several images in one turn, the proxy sends the first and
**drops the rest**, then prepends a notice to the prompt telling the model how
many were dropped and that they can be re-attached one at a time. This is
deliberate: a hard rejection would be unrecoverable, because the client rebuilds
the same delta on every retry, so the turn could never complete and the
conversation would wedge. Dropped counts are exposed as
`requests.images_dropped` in `/stats`.

Remote URLs are resolved and refused unless every answer is a public address —
loopback, link-local, private, CGNAT, and reserved ranges are rejected, and
redirects are not followed.

## Behaviour worth knowing

- **Thread resume.** A conversation is keyed by the client's session header
  (`session_id`, `x-session-id`, `x-session-affinity`, or `x-thread-id`, in
  that order), falling back to a hash of the first user message. The key maps
  to a server-side DeepSeek session id, so the remote side keeps the context
  and only the turn delta is uploaded.
- **Context-limit recovery.** When the backend reports
  `input_exceeds_limit` on a continuation, the proxy opens a new remote session
  and replays the full flattened transcript exactly once. It only fires before
  any content has been sent, and only for existing threads — a first turn has
  nothing to replay.
- **Tool-call repair.** A malformed or schema-violating tool call triggers a
  bounded correction turn instead of surfacing to the client, up to
  `tool_repair_max_retries`.
- **Rate limiting.** Turns that hit `rate_limit_reached` or "Messages too
  frequent" are retried with backoff *before* the first byte, so streaming
  clients can still be served. Work that cannot be retried fails cleanly with a
  typed error.
- **Stale cursors.** If a saved parent message id is rejected, the session is
  rebuilt from the transcript once rather than failing the turn.

## Architecture

```
crates/pow       DeepSeekHashV1 + multithreaded nonce search
crates/protocol  SSE parsing + fragment routing + OpenAI chunk shaping
crates/client    login, session, PoW header, streaming, image upload
crates/proxy     axum server, tool dialect, SQLite storage, auth, retry
corpus/          real captured SSE streams used as test fixtures
```

The dependency direction is strictly downward: `proxy` depends on `client`,
`client` on `protocol`, `protocol` on `pow`. Nothing in the lower crates knows
about HTTP serving.

The `corpus/` directory holds raw byte streams captured from
`POST https://chat.deepseek.com/api/v0/chat/completion`, paired with the
routing output the Python implementation produced. Tests assert byte-for-byte
equality against those expectations, which is what keeps the two
implementations honest about each other.

## DeepSeekHashV1

DeepSeek gates requests behind a proof-of-work that is *almost* SHA3-256: the
Keccak-f[1600] permutation skips round 0 and runs rounds 1..=23. No off-the-shelf
SHA3 crate produces this digest, so the permutation is implemented here; round
constants beyond `RC[0]` are standard Keccak values. See
`crates/pow/src/lib.rs`.

## Development

```bash
cargo build --release
cargo test              # 80+ tests, including corpus equality
cargo test -p proxy     # server-side behaviour only
```

Regenerate corpus expectations (requires the archived Python tree):

```bash
../deepseek/.venv/bin/python tools/gen_expected.py
```

### Logging

One pretty, human-readable format in both profiles.

| | prod (default) | debug (`DEEPSEEK_LOG=debug`) |
|---|---|---|
| spans | hidden | shown |
| target / file / line | omitted | shown |
| thread ids | omitted | shown |
| ANSI | only when stdout is a TTY | only when stdout is a TTY |

`RUST_LOG` overrides the filter entirely (a blank value is ignored rather than
meaning "log nothing"); `NO_COLOR` or `DEEPSEEK_LOG_ANSI=1|0` forces colour
off or on.

Per request you get `request in` → `new remote session` → `stream done` (or
`response out` for non-streaming), with thread id, model, tool-call count, and
elapsed milliseconds.

## License

MIT. See [LICENSE](LICENSE).
