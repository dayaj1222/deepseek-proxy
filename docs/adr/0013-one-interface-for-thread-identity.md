# ADR-0013: One interface opens every remote session

**Status:** accepted (2026-10-23)

## Context

A thread's remote identity is three things held together by convention: the
remote `session_id`, the `parent_message_id` cursor ring, and the account
binding. Four mechanisms mutated them, each remembering a different subset:

- `turn::prepare` created a session and called `remember`;
- `rebuild_session` created a session and called `replace`;
- `Pool::route` changed the account only;
- `Pool::failover` changed the account only, leaving `session_id` pointing at a
  session that lives under the old account.

The `ThreadState { .. }` literal was written out by hand at four production
sites, so adding a field was a compile-error sweep. Any new caller - forced
rebind (T-21), an admin route, a CLI - would have been a fifth mechanism with a
fifth subset.

Two defects sat on this seam and were found while mapping it:

1. `Pool::failover` moved the account without invalidating the session, so the
   mismatch was only discovered on the next completion, at the price of a
   rebuild plus a full replay.
2. `SqlitePersist::save` returned early when `parent_message_id` was `None`, so
   a fresh or rebuilt session was never persisted. A restart in that window
   opened a **second** remote session for a thread that already had one.

## Decision

`session::open_session(state, key, ThreadChange)` is the only way a remote
session is created. It resolves the account first (a rebind that cannot land
must fail before a session is created on the wrong account), then creates the
session, builds the state with `ThreadState::new`, installs it with `replace`,
and lets the adapter persist it.

Callers pass intent, never a `ThreadState`:

- `ThreadChange::Fresh` - first turn on a thread;
- `ThreadChange::Rebuild { reason }` - new session, same account;
- `ThreadChange::Rebind { target, reason }` - new session on another account.

`RebuildReason` is carried into the log line rather than a free-form string, so
"why did this thread get a new session" is answerable from the logs alone.
`Pool::route_forced` resolves `Next`/`Email` and writes both the in-memory
owner map and the durable binding, which is what makes a rebind stick: `route`
prefers the owner map and would otherwise undo it. An `Email` target that is
not healthy is refused rather than silently rerouted.

## Consequences

- Adding a field to `ThreadState` now touches one constructor.
- A fresh or rebuilt session has a durable row, so a restart no longer opens a
  second remote session.
- `route_forced` is the piece T-21 needs; the rebind trigger can be added
  without touching this module.
- `Threads::remember` is now test-only. Production installs state through
  `replace`, which also persists.
- Persisting `prev_parents` across restart is still unsolved and is deliberately
  not part of this change.
