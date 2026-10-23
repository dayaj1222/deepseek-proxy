# T-21 - Forced rebind: move a live thread to another account

**Status:** planned - Effort M - Depends on: nothing (reuses `rebuild_session`)
**Roadmap entry:** `PLAN.md` Phase D

## Problem

An account hits its rate limit at the moment the work matters. Today the only
outcomes are: wait out the backoff, or fail the turn. There is no way to say
"continue this conversation on my other account, with everything we have said so
far".

## What already exists (do not rebuild)

Most of this feature is already in the tree:

- `prompt::for_turn` builds **two** strings. `delta` is what a live session needs;
  `replay` is the flattened transcript **plus** the tool protocol, schemas and
  reminder (`prompt.rs:51-93`). The replay is exactly "all context sent by the
  client, plus the proxy's own prompt".
- `rebuild_session` (`main.rs:367`) creates a fresh remote session and swaps the
  thread state; callers then resend on `replay` (`main.rs:1631-1644`,
  `1694-1704`).
- Two existing triggers prove the path: context overflow, and a stale parent id
  with nothing left to rewind to (ADR-0008).

So the feature is **not** "carry context elsewhere". It is "force the rebuild,
and make it land on a different account".

## What is missing

### 1. The pool will undo the rebind

`Pool::route` (`pool.rs:531`) prefers the in-memory `owner` map. When a binding
is stale it only moves if the old account is **unhealthy**, and a rate-limit
cooldown is deliberately not a breaker trip (ADR-0007). So `route()` keeps
returning the throttled account.

Needed: `Pool::rebind(thread_id, email)` that writes both `store.bind()` and the
`owner` map, plus a way for `route()` to respect a fresh explicit choice. A pin
with an expiry (one turn) avoids permanently overriding routing.

### 2. Session ids and file ids are account-scoped

`session_id` belongs to the old account, so a rebind **must** create a new
session; reuse is impossible. Worse, `main.rs:1699` passes `&body.ref_file_ids`
into the rebuilt body. Within one account that is correct; across accounts it
names file ids the new account has never seen.

Rule: drop the current turn's file ids on a rebind, and attach the existing
image note so the model is told. Earlier turns are already `[image]`
placeholders in the replay, so nothing recoverable is lost.

### 3. Only before the first byte

Line 1723 already guards with `!pumped.emitted`. That is not incidental: after
SSE bytes reach the client, replaying into that stream is impossible. A rebind
is only valid **before** the model starts answering.

Therefore mid-stream rate limiting cannot be migrated. Say so in the README
rather than leaving a caller to discover it.

## Surface

### Trigger 1 - client header (primary)

```
x-deepseek-rs-rebind: next | fresh | <email>
```

- `next` - move to the least-crowded healthy account that is not the current one
- `<email>` - pin to that account for this turn
- `fresh` - rebuild the session on the same account (useful for a poisoned
  context; no account change)

Deterministic, debuggable, and matches the operator's intent: decide at the turn
that matters.

### Trigger 2 - CLI

```
deepseek-rs rebind <thread_key> [--account EMAIL]
```

Writes the pin so the **next** turn rebuilds. For rescue from a terminal when
the client cannot send headers.

### Trigger 3 - automatic, inside the existing rate-limit branch

```toml
[limits]
rate_limit_rebind       = true      # after the retry budget is spent, try another account
rebind_max_accounts     = 1         # hard stop against ping-pong
rebind_skip_replay_over = 200000    # replay bytes above which we do not migrate
```

Runs when the rate-limit branch (`main.rs:1723`) exhausts
`rate_limit_max_retries` **and** `!pumped.emitted`:

1. backoff retries exactly as today, unchanged
2. on exhaustion, if a healthy account exists that this turn has not used:
   rebind, then rebuild the session and resend on `replay`
3. only if that also fails: 429 with `Retry-After`

There is deliberately **no** `rate_limit_rebind_after = N` counter. `attempt`
is local to `stream_response` (`main.rs:1486`) and resets per request, so a
counter spanning two requests would need new per-thread persisted state and a
way to tell an operator retry from a new turn. `rate_limit_max_retries` already
is the threshold.

Default **on**, with `rebind_max_accounts = 1`: it fires only after the proxy has
spent its own retry budget, so it is a last resort rather than silent hopping.

### Why this is not "treat a 429 as a stale parent id"

Use the **mechanism**, not the **trigger**. `is_invalid_message_id` means the
backend rejected the cursor; a 429 means too fast. Conflating them makes the log
lie ("stale parent message id; rebuilding" for a throttle) and would run a
pointless cursor rewind, since a throttle has no cursor problem.

Instead factor the shared tail into one function with three callers and three
reasons:

```
rebuild_and_resend_on(state, key, reason, target_account, body, replay)
  <- is_invalid_message_id : "stale parent message id"
  <- is_context_limit      : "context overflow"
  <- rate limit exhausted  : "throttled; continuing on account B"
```

One replay implementation, honest log lines.

## Design notes

- **Reuse the rebuild path.** One `rebuild_session_on(state, key, reason, account)`
  that takes a target, with the existing two callers delegating to it. Do not add
  a second replay implementation.
- **Do not reset the old account.** A rebind must leave the previous account's
  failure counter and cooldown intact; it stays cooling down.
- **Honest failure.** If every account is throttled, return the same 429 as today
  with `Retry-After` - not a hang, and not a silent success.
- **Count it.** `requests.rebinds` in `/stats`, and one log line naming both
  accounts, so an operator can see the hop.
- **Turn lock** is already held for the whole turn (`threads.rs`), so no
  concurrent rebind race exists.

## Tests

- header `next` on a two-account mock: the rebuild creates a session on account B
  and the replay carries the earlier user message
- header with an explicit email pins to it; unknown email - 400
- header `fresh` keeps the account and still rebuilds
- rebind after the first byte is refused (the guard at main.rs:1723)
- `ref_file_ids` are empty after a cross-account rebind, and the image note is
  present
- the old account's breaker state is unchanged by the rebind
- auto mode with `rate_limit_rebind_after = 1` moves once, then stops at
  `rebind_max_accounts`
- every account throttled - 429 with `Retry-After`, no hang
- backoff retries still run first: with `rate_limit_max_retries = 2`, two
  upstream 429s are seen before any rebind
- a throttle that lands after the first byte does not rebind
- `rebind_skip_replay_over` exceeded - no migration, plain 429
- the log line for a throttle rebind names the throttle, not a stale cursor

## Not in scope

Mid-stream migration. Moving an in-flight tool call between accounts. Carrying
uploaded images across accounts (only the note). Per-key account allowlists.

## Risks

- **Double billing**: a migrated turn is billed on the new account. Document it.
- **Ping-pong**: two accounts each throttled by the other's replay. The cap plus
  default-off auto mode are the guards.
- **Silent context loss** if the replay is skipped by a future refactor. The test
  asserting the earlier user message survives is the guard.
- **Header trust**: an unauthenticated client could force hops. It already needs
  a valid bearer key to reach the handler, so this is a policy question only if
  per-key allowlists (T-04) later restrict accounts.
