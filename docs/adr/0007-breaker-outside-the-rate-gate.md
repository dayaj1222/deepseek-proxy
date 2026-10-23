# ADR-0007: Breaker state is not behind the rate-gate mutex

**Status:** accepted (2026-10-21)

## Context

A flapping account was retried on every turn. Login-path cooldowns did not stop
routing from handing it the next thread, so one bad account could burn the whole
retry budget while healthy accounts sat idle.

The obvious implementation records the turn outcome next to the rate gate, since
both are per-account state.

## Decision

Breaker state is a separate `std::sync::Mutex<BreakerState>` per account.

Two rules, each learned from a broken implementation:

1. **Never record under the rate-gate mutex.** The caller's `RateTicket` holds
   that guard for the whole turn, so recording the outcome behind the same lock
   deadlocked against its own ticket. Every HTTP test hung with zero CPU.
2. **A rate-limit cooldown is not a breaker trip.** Treating any cooldown as a
   trip made a throttled account fail over instantly and two accounts
   ping-ponged forever. Only an open breaker fast-fails; a plain cooldown is
   still slept out by the gate so the throttle retry works as designed.

A `std` mutex is sufficient because nothing in the critical section awaits.

## Consequences

- Failure counting covers 5xx, timeouts and dead logins; client 4xx does not
  count, because the caller is at fault, not the account.
- `/stats` exposes `breaker` (`closed` | `open` | `half_open`) and
  `retry_in_secs` per account. Those names are asserted by a test so a dashboard
  cannot silently break.
- `account_fail_threshold = 0` disables the breaker entirely.
