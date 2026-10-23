# ADR-0003: Every error uses the OpenAI envelope

**Status:** accepted (2026-10-21)

## Context

Auth failures returned a bare `detail` object and `ProxyError::payload` mapped
every status other than 400 to `server_error`. Official SDKs parse the body as
`{"error": {...}}`; anything else surfaces as a deserialization failure, so the
client reports the wrong problem entirely.

## Decision

Every error response - including 401, 404 for unknown routes, 405 for wrong
methods, and the 501 stubs - uses the OpenAI envelope:

```json
{"error": {"message": "...", "type": "...", "param": null, "code": "..."}}
```

`type` is derived from the status: 400/401/404 -> `invalid_request_error`,
429 -> `rate_limit_error`, 5xx -> `server_error`. 401 additionally carries
`WWW-Authenticate: Bearer`.

## Consequences

- This is the one intentional break from the Python port, which sent `detail`.
  Documented in the commit body.
- The 404 fallback and 405 handler are part of the envelope, not exceptions to
  it: a caller must never need to special-case a transport-level error.
- `code` is free-form and stable enough to assert on: `invalid_api_key`,
  `missing_api_key`, `not_supported`, `rate_limit_exceeded`.
