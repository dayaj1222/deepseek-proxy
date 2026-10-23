# ADR-0006: The account pool is swapped behind an `Arc`, never mutated in place

**Status:** accepted (2026-10-22)

## Context

Reload can add or remove DeepSeek accounts. A turn already running holds a rate
ticket and a bound account; mutating the pool under it would either block the
turn or change the account mid-flight. Both are unacceptable.

## Decision

`AppState.pool` is `RwLock<Arc<Pool>>` and callers take one `Arc<Pool>` snapshot
via `AppState::pool()`. A reload builds a new `Pool` and swaps the `Arc`. An
in-flight turn keeps the pool it started on; new turns get the new one. No lock
is held across an await.

A rebuild is skipped when nothing relevant changed: `PoolConfig` is `PartialEq`,
and the pool remembers the *configured* emails including ones that failed to log
in, so a permanently dead account does not force a login attempt on every
SIGHUP. A pool that fails to build leaves the running one installed - the same
retained-on-error rule as the config itself.

## Consequences

- Removal is not "draining" in the sophisticated sense: a turn using the old
  pool simply finishes. Thread bindings are in SQLite, so no conversation is
  stranded.
- Account changes are logged as added/removed, which is the only confirmation a
  reload happened besides `config.generation`.
- Reconciliation runs *after* the settings swap so a slow login never delays a
  timeout change.
