# API Key Authentication — Design Spec

Date: 2026-10-08
Status: Draft (awaiting review)

## Problem

The DeepSeek proxy deployed on Render is reachable by anyone with the URL.
`/v1/chat/completions` routes requests through the owner's DeepSeek accounts,
so an open endpoint lets strangers burn the owner's quota. The local proxy must
stay frictionless (no auth prompts, no keys) for day-to-day development.

## Goal

Add opt-in API-key authentication so that:

- The **owner** authenticates with a single user+password and can create, list,
  and revoke named API keys through a thin HTML management UI.
- **API-key holders** can call only the proxy endpoints (`/v1/*`).
- **Local runs stay exactly as they are today** — no auth, no UI, no extra setup.

Auth is enabled only on the deployment via configuration; the default is off.

## Non-Goals

- Per-key rate limits, quotas, scopes, or expiry.
- Audit logging of key usage beyond `last_used_at`.
- Multiple admin users, roles, or sessions/cookies.
- OAuth or third-party identity providers.

## Decisions (approved)

| Topic | Decision |
|---|---|
| Admin auth | `ADMIN_USER` + `ADMIN_PASS` env vars, HTTP Basic, constant-time compare |
| Key storage | SHA-256 hash at rest; raw key shown once at creation |
| Key metadata | name, key_prefix, created_at, last_used_at |
| Revocation | Hard delete |
| UI | Single self-contained HTML page served by the app, no build step |
| Key format | `sk-` + 43 base64url chars |
| Protected routes | `/v1/*` only; `/healthz`, `/readyz`, `/` stay open |
| Opt-in switch | `AUTH_ENABLED` (default `false`) |

## Architecture

### Configuration

New `Settings` fields (read via the existing `_env` / `_to_bool` helpers):

| Setting | Env var | Default | Notes |
|---|---|---|---|
| `auth_enabled` | `AUTH_ENABLED` | `false` | Master switch |
| `admin_user` | `ADMIN_USER` | `""` | Required when auth enabled |
| `admin_pass` | `ADMIN_PASS` | `""` | Required when auth enabled |

Validation in `__post_init__`:

- If `auth_enabled` is true and either `admin_user` or `admin_pass` is empty,
  raise `ValueError("ADMIN_USER and ADMIN_PASS must be set when AUTH_ENABLED=true")`.
- If `auth_enabled` is false, admin credentials are optional and ignored.

This makes the failure loud: an enabled deployment never starts in a
silently-open state.

### Data model

New logical table/collection `api_keys`:

| Field | Type | Purpose |
|---|---|---|
| `id` | string | stable identifier (uuid4 hex); the Mongo `_id` / SQLite PK |
| `key_hash` | string | SHA-256 hex of the raw key; unique |
| `key_prefix` | string | e.g. `sk-aB3` — display only, never the full key |
| `name` | string | operator-supplied label |
| `created_at` | float | epoch seconds |
| `last_used_at` | float \| null | epoch seconds, updated on successful use |

`id` is a generated uuid rather than the hash so that the hash (a credential
fingerprint) is not exposed in URLs or UI.

### Backend interface

Add to the `StorageBackend` protocol and implement in both backends:

```
insert_api_key(record: dict) -> None
list_api_keys() -> list[dict]              # newest first
find_api_key_by_hash(key_hash: str) -> dict | None
delete_api_key(key_id: str) -> bool        # True if a row was removed
touch_api_key_used(key_id: str, when: float) -> None
```

- `SqliteBackend`: new `api_keys` table created in `open()`; local tests use it.
- `MongoBackend`: new `api_keys` collection with a unique index on `key_hash`.

Records returned to callers never include `key_hash` except the internal lookup
path; the list API strips it.

### Key generation

```
def generate_key() -> str:
    return "sk-" + secrets.token_urlsafe(32)   # 32 bytes -> 43 base64url chars
```

`secrets` (CSPRNG) only. `key_hash = hashlib.sha256(raw.encode()).hexdigest()`.

### Auth modules

New file `deepseek_proxy/auth.py` containing pure, testable helpers:

- `generate_key() -> str`
- `hash_key(raw: str) -> str`
- `check_admin(user, password, expected_user, expected_pass) -> bool`
  (uses `hmac.compare_digest` on both fields, constant-time)
- `parse_basic(header) -> (user, pass) | None`
- `parse_bearer(header) -> str | None`

FastAPI dependencies in the same module:

- `require_admin` — HTTP Basic; 401 with `WWW-Authenticate: Basic` on failure;
  503 if auth is enabled but admin creds are somehow unset (defensive).
- `require_api_key` — reads `Authorization: Bearer sk-...`; looks up the hash;
  401 on missing/unknown. When `settings.auth_enabled` is false, it is a no-op
  that returns immediately (so it can be attached unconditionally).

### Routes

Registered only when `auth_enabled` is true:

| Method | Path | Auth | Purpose |
|---|---|---|---|
| GET | `/admin` | Basic | HTML management UI |
| GET | `/admin/keys` | Basic | JSON list (no hashes) |
| POST | `/admin/keys` | Basic | Create; body `{name}`; returns `{id, name, key, key_prefix, created_at}` with raw `key` **once** |
| DELETE | `/admin/keys/{id}` | Basic | Revoke (hard delete); 404 if absent |

Always registered:

| Method | Path | Auth |
|---|---|---|
| GET/POST | `/v1/chat/completions` | Bearer (no-op if disabled) |
| GET | `/v1/models` | Bearer (no-op if disabled) |
| GET | `/healthz`, `/readyz`, `/` | none |

### Management UI

One HTML string in `deepseek_proxy/admin_ui.py`, returned by `GET /admin`.
Inline CSS/JS, no external assets, no framework. The page:

- Lists keys in a table: name, prefix, created, last used.
- Has a "Create key" form (name only). On success it shows the raw key once in
  a copyable box with a warning that it will not be shown again.
- Has a "Revoke" button per row.
- Uses `fetch()` against `/admin/keys`; the browser supplies the Basic-auth
  credential after the initial 401 challenge.

The raw key is held only in page memory until dismissed; never written to
localStorage.

### Request flow (`auth_enabled = true`)

1. Client sends `POST /v1/chat/completions` with `Authorization: Bearer sk-...`.
2. `require_api_key` extracts the token, hashes it, looks it up.
3. Unknown/missing → 401 `{"error": {"message": "invalid api key", ...}}`.
4. Known → update `last_used_at` (best-effort) and continue to the handler.

### Request flow (local, `auth_enabled = false`)

Identical to today. `require_api_key` returns immediately; `/admin` routes are
not registered (404); no credentials are needed.

## Error Handling

- 401 for missing/invalid API key and missing/invalid admin Basic auth.
- 404 for `DELETE /admin/keys/{id}` when the id does not exist.
- 400 for a create request with an empty/missing `name`.
- Startup `ValueError` if auth is enabled without admin credentials.
- `last_used_at` update failures are logged and swallowed — they must never
  fail a user request.

## Testing

SQLite backend for all tests (the Mongo path shares the same logic).

- `auth.py` units: key format/entropy prefix, `hash_key` determinism,
  `check_admin` accepts correct and rejects wrong/partial creds,
  `parse_basic`/`parse_bearer` edge cases.
- Backend units: insert/list/find/delete/touch round-trip on SQLite; list
  excludes `key_hash`; duplicate hash rejected.
- Route integration (TestClient, `auth_enabled=true`):
  - `/v1/models` without key → 401; with valid key → 200.
  - Revoked key → 401.
  - `/admin/keys` without Basic → 401; with Basic → 200.
  - create → key works; delete → key stops working.
- Regression (`auth_enabled=false`): `/v1/models` and a chat completion return
  200 with **no** Authorization header; `/admin` → 404.

## Deployment

`render.yaml` adds:

```yaml
- key: AUTH_ENABLED
  value: "true"
- key: ADMIN_USER
  sync: false
- key: ADMIN_PASS
  sync: false
```

`.env.example` documents `AUTH_ENABLED=false` as the local default.

## Open Questions

None. All decisions above are approved.
