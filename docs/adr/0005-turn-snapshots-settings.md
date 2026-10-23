# ADR-0005: A turn snapshots settings once

**Status:** accepted (2026-10-22)

## Context

Hot reload means settings can change at any moment, including in the middle of a
streaming turn. A deadline that moves under a running stream, or a retry budget
that changes after the first failure, makes behaviour unreproducible and makes
"which config did this turn use?" unanswerable.

## Decision

`Settings` sits behind `Arc<SettingsHandle>`. A turn calls `state.settings.get()`
**once**, at the top, and uses that snapshot for its whole life. Everything
inside the turn - deadlines, retry budgets, repair limits, heartbeats - reads
from that snapshot, never from `state.settings` again.

`get()` is a single `Arc` clone under a read lock that is never held across an
await. No `arc-swap` dependency, and no possibility of a lock living through a
stream.

A config that fails to parse installs nothing: the running config stays, the
error is logged, and it is exposed as `/stats.config.last_error` without
advancing `generation`. Restart-only keys (`host`, `port`, `db_path`,
`image_max_bytes`) are reported as `not reloadable; restart required` instead of
being silently ignored.

## Consequences

- Adding a settings read inside a turn loop is a bug unless it reads
  `turn_settings`. The comment at the snapshot site says so.
- `backoff_delay` takes `&Settings` instead of `&AppState` to make the snapshot
  the only possible source.
- The account pool follows the same rule with a different mechanism: see
  ADR-0006.
