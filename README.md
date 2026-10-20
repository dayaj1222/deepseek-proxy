# deepseek-rs

Single-binary Rust port of the Python `aiodeepseek` client + `deepseek_proxy`.

**This is the canonical implementation.** The Python proxy this was ported
from is preserved unchanged on the `python-archive` branch (tag
`python-final`) for reference and for the captured SSE corpus — not for
running.

## Status

| Crate | What | State |
|---|---|---|
| `pow` | `DeepSeekHashV1` proof-of-work | done, verified vs live server |
| `protocol` | DeepSeek SSE fragment router -> OpenAI chunks | done, byte-identical to Python |
| `client` | login, session, PoW header, streaming | done, live-verified |
| `proxy` | axum HTTP shell, tools, SQLite, auth, retry | done, live-verified |

## Behaviour worth knowing

- **Login tokens are cached** in SQLite (`account_tokens`). A restart reuses
the cached bearer token instead of logging in again; a token the server
rejects is dropped and re-fetched on the next recover, so a stale cache
self-heals. `DEEPSEEK_TOKEN_<i>` still wins when set, since it is explicit.
- **Context-limit recovery.** When DeepSeek answers a continuation turn with an
`input_exceeds_limit` hint, the proxy creates a new remote session and replays
the full flattened transcript once, mirroring the Python proxy. It only fires
before any content has been sent, and only for existing threads (a first
turn has nothing to replay).

## `DeepSeekHashV1`

SHA3-256 with one mutation: Keccak-f[1600] **skips round 0** (runs rounds
1..=23). No off-the-shelf SHA3 crate works; the permutation is implemented
here. Round constants beyond `RC[0]` are standard Keccak values.

See `crates/pow/src/lib.rs` and `crates/protocol/tests/corpus.rs`.

## Layout

```
crates/pow       DeepSeekHashV1 + multithreaded nonce search
crates/protocol  SSE parsing + fragment routing + OpenAI chunk shaping
crates/proxy     axum server, tool dialect, SQLite storage, auth
corpus/          real captured SSE streams used as test fixtures
python-archive   (branch) the Python implementation this replaced
```

## Corpus

`corpus/*.sse` are raw byte streams captured from
`POST https://chat.deepseek.com/api/v0/chat/completion`. `corpus/*.expected.json`
holds the routing output produced by the Python implementation; the Rust tests
assert byte-for-byte equality against them.

Regenerate expectations with:

```bash
../deepseek/.venv/bin/python tools/gen_expected.py
```

## Build / test

```bash
cargo build --release
cargo test
```

## CLI

One binary, `deepseek-rs`. `serve` is the only long-running command; everything
else is short-lived.

```
SERVER
  serve                       run in the foreground (Ctrl-C to stop)
  start                       run in the background, wait until healthy
  stop                        SIGTERM (SIGKILL after 15s)
  restart                     stop, then start
  status                      pid, url, uptime, account count
  health                      one-shot health check (exit 1 when down)
  stats [--json]              counters, per-account state, db counts

LOGS
  logs [-n N] [-f] [--path P] last N lines (default 50); -f follows

ACCOUNTS
  accounts                    list accounts + where they come from
  accounts add <email> <password>
  accounts add <email> --token <token>
  accounts rm <email>

API KEYS
  keys                        list (name, prefix, id, created)
  keys add <name> <key>       alias: key <name> <key>
  keys rm <name|id>

MISC
  check | url | version | help
```

Accounts can live in two places: the environment (`DEEPSEEK_EMAIL_<i>` etc.,
which also reads `../deepseek/.env`) and the `accounts` table in SQLite, written
by `accounts add`. Environment entries win on an email collision, and neither
kind needs the other to exist.

Background mode keeps a pidfile and a log file under `DEEPSEEK_RUNTIME_DIR`
(default `${XDG_STATE_HOME:-~/.local/state}/deepseek-rs`). `start` only returns
success once `/healthz` answers, so a script can chain on it. `serve` claims the
pidfile too, so a second instance fails immediately instead of fighting over the
SQLite lock.

`GET /stats` backs both `stats` and `status`:

```json
{"pid":0,"uptime_secs":0,"model":"...","thinking":false,"search":false,
 "requests":{"total":0,"ok":0,"errors":0,"auth_failed":0,"retries":0,"tool_calls":0},
 "threads":{"in_memory":0},
 "db":{"threads":0,"bindings":0,"api_keys":0,"accounts":0,"cached_tokens":0},
 "accounts":[{"email":"...","healthy":true,"bound_threads":0,"waiting":0,"cooldown_ms":null}]}
```

With the server down, `stats` falls back to database counts and says so on
stderr.

## Logging

One pretty, human-readable format in both profiles (`crates/proxy/src/log.rs`).

| | prod (default) | debug (`DEEPSEEK_LOG=debug`) |
|---|---|---|
| spans | hidden | shown |
| target / file / line | omitted | shown |
| thread ids | omitted | shown |
| ANSI | only when stdout is a TTY | only when stdout is a TTY |

`RUST_LOG` overrides the filter entirely (a blank value is ignored rather than
meaning "log nothing"); `NO_COLOR` or `DEEPSEEK_LOG_ANSI=1|0` forces colour off
or on.

Per request you get `request in` -> `new remote session` -> `stream done` (or
`response out` for non-streaming), with thread id, model, tool-call count, and
elapsed milliseconds.
