# ADR-0004: `x-request-id` is minted outermost and lives in a tracing span

**Status:** accepted (2026-10-21)

## Context

With one log line per turn and failover, a support question is "what happened to
*this* request". A correlation id only helps if it is on the response and on
every log line without threading a parameter through every function.

## Decision

The middleware layer is the outermost layer, so the id exists for 404/405 and for
errors as well as successes. An inbound `x-request-id` is echoed; otherwise one
is minted. The id is put in a tracing span, so any event logged while handling
the request carries `req=` without extra plumbing.

## Consequences

- Client-supplied ids are trusted verbatim. They are opaque to us and appear only
  in logs and a response header - not in SQL, not as a file name.
- Because the span is outermost, the id is available to the pool, the turn and
  the shutdown path alike.
- The completed-turn log line reads the account from the `RateTicket`, not from
  the routing decision, because only the ticket knows which attempt served the
  turn after a failover.
