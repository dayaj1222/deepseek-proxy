# T-04 - API-key metadata: label, expiry, per-key model allowlist, RPM

**Status:** planned · **Effort:** M · **Depends on:** nothing
**Roadmap entry:** [`PLAN.md`](../../PLAN.md) Phase D

## Problem

`api_keys` stores a name and a SHA-256 hash. That is enough to say yes or no,
and nothing else. An operator cannot answer "which key is this?", "when did it
last work?", "should it stop working on Friday?". A single key therefore ends up
shared, and revoking it means breaking every consumer at once.

## Surface

### Schema (additive, migration per PLAN rule 8)

| Column | Type | Meaning |
|---|---|---|
| `expires_at` | `INTEGER NULL` | unix seconds; `NULL` = never |
| `models` | `TEXT NULL` | JSON array of allowed model names; `NULL` = all |
| `rpm` | `INTEGER NULL` | requests/minute for T-05; `NULL` = unlimited |
| `disabled` | `INTEGER DEFAULT 0` | 1 = reject even if unexpired |
| `last_used_at` | `INTEGER NULL` | debounced, ≤ 1 write/min/key |
| `created_at` | `INTEGER` | set once at insert |

Migration: `CREATE TABLE IF NOT EXISTS` for fresh DBs plus guarded `ALTER TABLE
ADD COLUMN` for existing ones, inside `Store::migrate`. A migration test opens a DB
created with the *previous* schema and asserts it still verifies a key.

### CLI

```
deepseek-rs keys add <name> <key> [--expires 30d] [--models a,b] [--rpm N]
deepseek-rs keys disable <name>
deepseek-rs keys enable  <name>
deepseek-rs keys list                 # name, created, expires, last used, models
```

The key itself is never printed after `add`. `--expires` accepts `30d`, `12h`,
`2026-12-01`.

### HTTP

- `verify_api_key` returns `Option<KeyId>` instead of `bool`, so handlers can
  attribute usage (T-05) without a second lookup.
- Expired -> 401 `expired_api_key`; disabled -> 401 `disabled_api_key`;
  model not in the allowlist -> 403 `model_not_allowed` with `param: "model"`.
- "No keys registered = open" stays exactly as it is: the first `keys add` is
  what turns auth on.

## Decisions to make while building

1. **Debounce location.** `last_used_at` must not become a write on the request
   hot path (see the SQLite risk in `PLAN.md`). Plan: keep a per-key `AtomicU64`
   of the last written minute; the aggregator flush (T-05) writes it. If T-05 is
   not built yet, a tiny background task flushes once a minute.
2. **Model allowlist matching** is exact string match against the resolved model,
   not a glob. A glob invites a `*` that silently disables the feature.
3. **`expires_at` in the past on `add`** is an error, not a silently dead key.

## Tests

- migration from the previous schema (open old DB, verify a key still works)
- expired key -> 401 with `code: "expired_api_key"`
- disabled key -> 401, `enable` restores it
- disallowed model -> 403 `model_not_allowed`, allowed model passes
- `last_used_at` written at most once per minute for N requests
- hash is still not reversible (no plaintext column, grep the schema)

## Not in scope

Rate limiting itself (T-05), usage reporting (T-05), key rotation with overlap.

## Risks

- **Auth regression** is the worst outcome here: a bug that makes `verify_api_key`
  return `None` on a valid key locks the operator out. The migration test and a
  "no keys registered = open" test both guard it.
