# T-22 - One interface for thread identity changes

**Status:** in progress - Effort M - Depends on: nothing (unblocks T-21)
**Roadmap entry:** `PLAN.md` Phase D

## Problem

A thread's remote identity is three things held together by convention: the
remote `session_id`, the `parent_message_id` cursor ring, and the account
binding. Today four separate mechanisms mutate them, and each one remembers a
different subset:

| Site | What it changes |
| --- | --- |
| `turn::prepare` (`turn.rs:104-133`) | new session, `remember` |
| `rebuild_session` (`main.rs:367-397`) | new session, `replace` |
| `Pool::route` (`pool.rs:531`) | account only |
| `Pool::failover` (`pool.rs:628`) | account only, **session left stale** |

Two consequences are visible in the code, not hypothetical:

1. `Pool::failover` moves the account but leaves `session_id` pointing at a
   session that lives under the old account. The mismatch is only discovered on
   the next completion, which then costs a rebuild plus a full replay.
2. `SqlitePersist::save` returns early when `parent_message_id` is `None`
   (`threads.rs:71-74`), so a **fresh or rebuilt** session is never persisted.
   A restart in that window silently creates a second remote session.

Any new caller - forced rebind (T-21), an admin route, a CLI - would be a fifth
mechanism with a fifth subset. The `ThreadState { .. }` literal is written out
by hand at four production sites (`main.rs:386`, `main.rs:1885`,
`turn.rs:125`, `threads.rs:191`); every new field is a compile-error sweep.

## The interface

One function owns "this thread's remote identity changed":

```rust
// session.rs
pub enum AccountTarget { Current, Next, Email(String) }

pub enum RebuildReason {
    ContextLimit,
    StaleCursor,
    Directive,
    Throttled,
    Manual,
}

pub enum ThreadChange {
    /// No prior session: first turn on this thread.
    Fresh,
    /// New remote session, same account.
    Rebuild { reason: RebuildReason },
    /// New remote session on another account.
    Rebind  { target: AccountTarget, reason: RebuildReason },
}

pub async fn open_session(
    state: &AppState,
    key: &str,
    change: ThreadChange,
) -> Result<ThreadState, ProxyError>;
```

`open_session` does, once, in order:

1. resolve the account - `route(key)` for `Current`, `route_forced` for
   `Next`/`Email`;
2. acquire the account's rate slot;
3. `create_chat_session()`;
4. `ThreadState::new(session_id)`;
5. `threads.replace(key, state.clone())`;
6. persist the row, now permitted with a `NULL` parent.

Callers stop touching the struct. `ThreadState::new` becomes the only place
that knows the field list, so adding a field touches one constructor instead
of four literals.

### `Pool::route_forced`

```rust
pub async fn route_forced(
    &self,
    thread_id: &str,
    target: &AccountTarget,
) -> Result<Arc<Connection>, PoolError>;
```

- `Next` - least-crowded healthy account other than the current one.
- `Email` - that account, **refused if `!healthy()`** rather than silently
  routing elsewhere.
- On success, writes both the in-memory `owner` map and `store.bind`, so a
  later `route()` honours the choice. This is the missing piece T-21 also
  needs; T-22 provides it.

## Migration

Each step is a separate commit and leaves the tree green.

1. `ThreadState::new` + `session.rs` with `open_session`, no callers yet.
2. `Pool::route_forced`.
3. `turn::prepare` -> `ThreadChange::Fresh`.
4. `rebuild_session` -> `ThreadChange::Rebuild { reason }`; keep the function
   as a thin wrapper in this commit so the two call sites do not move twice.
5. Fix `SqlitePersist::save` so a `NULL` parent still writes the row.
6. Delete the wrapper once T-21 lands its third caller.

## Behaviour that must not change

- A continuation still sends only `delta_messages`; `open_session` is not on
  that path at all.
- `REWIND_DEPTH = 5` and "rewind before rebuild" (ADR-0008) stay exactly as
  they are; `Rebuild` is the same tail it is today.
- A rate-limit cooldown still does not trip the breaker (ADR-0007).
  `route_forced` consults `healthy()`, which is unchanged by cooldown.
- `ref_file_ids` are account-scoped and must be dropped by the **caller** on a
  cross-account `Rebind`, not inside `open_session` (which cannot know whether
  the caller intends to re-upload).

## Tests

- `open_session(Fresh)` creates one session and a durable row.
- `open_session(Rebuild)` replaces the state and clears the cursor ring.
- `open_session(Rebind{Next})` lands on a different healthy account and the
  new binding survives a `route()`.
- `open_session(Rebind{Email})` refuses an unhealthy account instead of
  falling back.
- A rebuilt session is visible to `resume()` after a simulated restart
  (the `NULL`-parent fix).
- Existing `threads.rs` tests stay green; `a_rebuilt_session_forgets_the_old_cursors`
  must not depend on the save early-return.

## Out of scope

- The trigger policy (when to rebind) - that is T-21.
- The admin route and CLI (`POST /admin/rebind`, `deepseek-rs rebind`) - built
  on this interface, planned separately.
- Persisting `prev_parents` across restart (a real gap, but a different one).
