# ADR-0010: Shutdown is bounded, readiness first

**Status:** accepted (2026-10-22)

## Context

Graceful shutdown waited for in-flight SSE with no deadline. One stalled upstream
stream blocked `stop` until SIGKILL at a hard-coded 15 s, and the CLI could not
tell a slow drain from a hung process.

## Decision

On SIGTERM/SIGINT:

1. `/readyz` reports 503 immediately, so a load balancer stops sending work.
2. In-flight turns get `[limits] shutdown_grace_secs` (default 20) to finish.
3. Whatever is left is cancelled; cursors are persisted for turns that had one.
4. The process exits 0.

`deepseek-rs stop` waits `grace + 5 s` before escalating, derived from the config
instead of a hard-coded number.

## Consequences

- A turn cut off mid-stream leaves the cursor where the last commit put it. That
  is the pre-existing contract (see the risk register in `PLAN.md`): a truncated
  client view must not leave a *stale* cursor, and it does not.
- `shutdown_grace_secs` is reloadable, but a value changed after SIGTERM is
  ignored - the drain has already started with its snapshot.
- Usage flushing (T-05, not yet built) must hook in before step 4.
