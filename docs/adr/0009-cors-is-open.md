# ADR-0009: CORS is open; the bearer key is the boundary

**Status:** accepted (2026-10-22)

## Context

A browser UI could not call the API at all: the preflight got 405 and no response
carried `Access-Control-Allow-Origin`. Without CORS the surface is unusable from
the one client class that cannot avoid preflight.

## Decision

The preflight is answered and every response carries
`Access-Control-Allow-Origin: *`.

Authorization is the bearer key, not the origin. A browser that has no key gets
401 from the handler regardless of what the origin allowlist says; a browser that
has the key can send it from anywhere anyway. An allowlist would therefore add a
way to break a working deployment without adding a guarantee.

## Consequences

- This is not a "CORS disables auth" change: no credential is sent implicitly,
  there are no cookies, and the `Authorization` header still decides.
- If cookie-based session auth is ever added, this decision must be revisited -
  `*` plus credentials is exactly the case the CORS spec forbids.
- Deployments that must restrict origins should do it at the reverse proxy, where
  the whole request is visible.
