# T-18 - Optional TLS and the open-exposure guard

**Status:** planned · **Effort:** M · **Depends on:** nothing
**Roadmap entry:** [`PLAN.md`](../../PLAN.md) Phase E

## Problem

The server binds plain HTTP. Behind a reverse proxy that is fine and is the
recommended deployment; exposed directly it means an API key crosses the network
in clear text. Separately, a server with **no API keys registered** accepts
anyone - which is the right default for `localhost` and a dangerous one for
`0.0.0.0`.

The second problem is the more likely accident, so it comes first.

## Part 1 - exposure guard

At startup, if the bind address is **not** loopback **and** `api_keys` is empty:

- log a loud warning, and
- **refuse to start** unless `[proxy] allow_open = true` (or `ALLOW_OPEN=1`).

`allow_open` defaults to `false`. A non-loopback bind with keys registered starts
normally: the guard is about an *open* server, not about a public one.

This is a behaviour change for anyone currently running open on a public
interface, so it is called out in the README and in the release notes, and the
error message names the exact flag that overrides it.

## Part 2 - TLS

`[proxy] tls_cert` / `tls_key`, rustls via `axum-server` or `tokio-rustls`. Both
set -> serve HTTPS; neither -> HTTP; exactly one -> refuse to start with a clear
message (a half-configured TLS is never silently plaintext).

When TLS is terminated here rather than by a proxy, HSTS is off by default and
`[proxy] hsts = true` turns it on; a self-signed certificate plus HSTS is a trap
for the next user of that origin.

## Tests

- config matrix: loopback + no keys -> starts; `0.0.0.0` + no keys -> refuses;
  `0.0.0.0` + no keys + `allow_open` -> starts with a warning;
  `0.0.0.0` + one key -> starts
- only `tls_cert` set -> refuses to start, error names `tls_key`
- TLS handshake against a self-signed certificate generated in the test
- `http://` to a TLS listener fails cleanly rather than hanging

## Not in scope

ACME/automatic certificates, client certificates, mTLS, SNI-based multi-tenancy.

## Risks

- **Refusing to start is a breaking change** for an existing public open
  deployment after an upgrade. Mitigation: loud, specific error naming the
  override; README and release note; the check is on the *effective* bind
  address, so a proxy-only deployment never sees it.
- **TLS in-process** adds a dependency tree. If `axum-server` pulls in more than
  is comfortable, TLS stays out and the README keeps recommending a reverse
  proxy - the guard is the part that matters.
