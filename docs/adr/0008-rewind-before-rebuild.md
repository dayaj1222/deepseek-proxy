# ADR-0008: A stale cursor is rewound before the session is rebuilt

**Status:** accepted (2026-10-21)

## Context

When DeepSeek rejects the saved `parent_message_id` (`biz_code 26`, "invalid
message id") the cursor is gone. The only recovery used to be a full rebuild: a
new remote session plus a replay of the entire transcript. On a long thread the
replay is itself what trips the upstream throttle, so a recoverable turn turned
into a 429 loop - the recovery was worse than the fault.

## Decision

Each thread keeps the last `REWIND_DEPTH` (3) committed cursors in memory. On a
rejection the proxy pops one, persists it, prepends `LOST_REPLY_NOTE` and resends
the same delta on the same session.

The rebuild path stays as the second tier, unchanged, for when there is nothing
to step back to: a fresh process, the first turn, or a session that was itself
already rebuilt (a rebuild clears the ring, because the old cursors belong to a
remote session that no longer exists).

One rewind per turn. A retry that fails again falls through to rebuild rather
than walking the ring down in a loop.

## Consequences

- The in-memory ring is not persisted. After a restart the first rejected cursor
  costs a replay - the pre-rewind behaviour - which is acceptable and bounded.
- `LOST_REPLY_NOTE` is prepended to the delta, not the transcript, so the client
  never sees it and the stored transcript stays clean.
- Every `ThreadState { .. }` literal must include `prev_parents`; a compile error
  is the guard.
- The mock upstream can emit a raw non-SSE JSON body (`Step::NonSse`) so the
  rewind is covered end to end, including the assertion that the retry goes out
  on the *previous* cursor (`parents() == [None, Some(2), Some(3), Some(2)]`).
