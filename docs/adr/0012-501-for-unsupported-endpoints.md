# ADR-0012: Unsupported OpenAI endpoints answer 501, never a fake success

**Status:** accepted (2026-10-22)

## Context

`/v1/embeddings`, the legacy `/v1/completions`, `/v1/images/*` and `/v1/audio/*`
answered 404. A capability probe cannot distinguish "this server is old" from
"this route does not exist in the OpenAI API", and a client that falls back on
404 may fall back to something worse.

## Decision

One `not_supported` handler answers all of them with 501 and the OpenAI envelope,
`code: "not_supported"`, pointing at the README's "Not supported" section.

A fabricated embedding or a silently ignored audio request is worse than a
refusal: a fake embedding poisons a RAG index invisibly, and the damage surfaces
long after the request.

## Consequences

- 501 means "not implemented", which is exactly true and is distinguishable from
  404 (no such route) and 400 (bad request).
- Adding a real implementation of any of these is additive: the route changes
  handler and the envelope disappears for that path only.
- The README lists what is not supported. If that list and the routes drift, the
  route table is authoritative.
