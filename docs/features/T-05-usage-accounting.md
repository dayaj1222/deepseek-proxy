# T-05 - Usage accounting and per-key rate limit

**Status:** planned · **Effort:** M · **Depends on:** T-04
**Roadmap entry:** [`PLAN.md`](../../PLAN.md) Phase D

## Problem

`/stats` reports process-lifetime counters. Nothing survives a restart, and
nothing is attributed to the key that caused the traffic. With per-key `rpm`
stored in T-04 but not enforced, a single misbehaving client can exhaust the
DeepSeek rate budget for everyone.

## Surface

### Schema (additive)

```sql
CREATE TABLE IF NOT EXISTS usage (
  key_id            INTEGER NOT NULL,
  day               TEXT    NOT NULL,  -- YYYY-MM-DD, UTC
  requests          INTEGER NOT NULL DEFAULT 0,
  prompt_tokens     INTEGER NOT NULL DEFAULT 0,
  completion_tokens INTEGER NOT NULL DEFAULT 0,
  errors            INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (key_id, day)
);
```

`key_id` is T-04's `KeyId`. Usage for an unauthenticated (open) server is
recorded against a reserved id `0` so the numbers are still comparable.

### CLI

```
deepseek-rs usage [--key NAME] [--days N] [--json]
```

### Enforcement

An in-memory token bucket per key (`rpm` from T-04). On exhaustion: 429 with the
OpenAI envelope, `code: "rate_limit_exceeded"`, and `Retry-After` in seconds -
the same shape the upstream 429 already produces, so a client needs no new
branch.

## Design

**Writes are batched, never on the hot path.** An in-memory aggregator
(`HashMap<KeyId, Bucket>`) accumulates deltas; a task flushes every 30 s and once
more on shutdown (hook before step 4 of ADR-0010). A flush is a single upsert
statement per key, not one per request. If the process dies between flushes the
in-flight counts are lost - documented in the README, because a usage table that
silently overstates is worse than one that understates by 30 s.

**Counters come from the turn, not from the response.** The same numbers the
`turn done` log line prints (`usage_block`) are what get recorded, so a log line
and a usage row can never disagree.

## Tests

- bucket refills with paused tokio time; N requests at `rpm: 2` -> third gets 429
- `Retry-After` is present and sane on the 429
- flush on shutdown writes the accumulated counts (spawn, request, SIGTERM, reopen DB)
- usage survives a restart (write, restart, `usage` returns the same numbers)
- a failing turn increments `errors` but not `completion_tokens`

## Not in scope

Daily quotas, cost in currency, billing export, per-key token limits.

## Risks

- **SQLite write contention** is the stated risk in `PLAN.md`. Guard: batch +
  debounce, never write per request, and a single writer task.
- **Clock**: `day` is UTC and derived from one clock read at flush time. A day
  boundary crossing during a long turn attributes the turn to the flush day. This
  is accepted and documented; per-request day stamping would put a clock read on
  the hot path for no real gain.
