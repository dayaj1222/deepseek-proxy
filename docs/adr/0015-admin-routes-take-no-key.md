# ADR-0015: The admin routes take no API key

**Status:** accepted (2026-10-28)
**Amends:** ADR-0009 (the bearer key was the only authorization boundary)

## Context

`GET /admin/throttled` and `POST /admin/rebind` were behind the same API-key
check as the chat route, on the reasoning that "a valid key may not hop accounts
for free". In practice the only caller is `deepseek-rs rebind` run by the
operator on the same machine, and having to export `DEEPSEEK_API_KEY` or pass
`--key` for it was friction with no one else to protect against.

## Decision

Both routes no longer call `authorize`. The chat route keeps its check.
`deepseek-rs rebind` still sends a bearer token if one is supplied; the server
ignores it. The test that pinned the 401 became `admin_routes_need_no_key`.

## Alternatives considered

- **Keep the key.** Rejected by the owner (the friction was the complaint).
- **Allow only loopback callers.** Not done. It would keep the convenience and
  close the exposure, but it is not implemented yet.
- **A separate admin token.** Not done; more config for a single-operator tool.

## Consequences

- **Accepted risk:** the proxy binds `0.0.0.0` by default, so anyone who can
  reach the port can list throttled threads (thread ids, account emails) and
  force rebinds, each of which replays a whole transcript. Do not expose the
  port to an untrusted network without a reverse proxy or firewall in front
  (see T-18 for the planned exposure guard).
- If the proxy is ever multi-tenant, this decision must be reversed.
- The cheap hardening is to restrict these two routes to loopback; do that
  before widening exposure.

## Verification

`admin_routes_need_no_key` in `crates/proxy/tests/http.rs`.
