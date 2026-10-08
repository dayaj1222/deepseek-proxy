# API Key Authentication Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add opt-in API-key authentication (admin Basic-auth management UI + `sk-` keys gating `/v1/*`), off by default so local runs are unchanged.

**Architecture:** A single `AUTH_ENABLED` setting gates everything. When false, `/v1/*` has no auth dependency and `/admin` routes are never registered. When true, a new `api_keys` table/collection (behind the existing `StorageBackend` seam) stores SHA-256 hashes, and FastAPI dependencies enforce Basic auth on `/admin/*` and Bearer keys on `/v1/*`.

**Tech Stack:** Python 3.12, FastAPI, `secrets`/`hashlib`/`hmac` (stdlib), pymongo (existing), SQLite (tests).

**Spec:** `docs/superpowers/specs/2026-10-08-api-key-auth-design.md`

## Global Constraints

- Key format: `"sk-" + secrets.token_urlsafe(32)` (43 base64url chars after `sk-`).
- Hashing: `hashlib.sha256(raw.encode()).hexdigest()` (lowercase hex).
- Admin compare: `hmac.compare_digest` on both user and pass.
- New setting names: `AUTH_ENABLED` (bool, default false), `ADMIN_USER`, `ADMIN_PASS` (strings, default `""`).
- `api_keys` fields: `id` (uuid4 hex), `key_hash`, `key_prefix`, `name`, `created_at` (float), `last_used_at` (float|null).
- Raw key returned exactly once, on create. Never logged, never stored.
- `/v1/*` auth is a no-op when `auth_enabled` is false.
- `/admin` routes registered only when `auth_enabled` is true.
- Local test command: `.venv/bin/python -m pytest tests/ -q`.
- Lint: `.venv/bin/ruff check .`

## Review Focus

- **Auth disabled (default):** a request with no `Authorization` header to `/v1/models` must return 200, and `GET /admin` must return 404 — the exact local behavior the owner demanded.
- **Enabled without admin creds:** startup must raise, never serve open. Test imports `Settings(auth_enabled=True, admin_user="", admin_pass="")` and expects `ValueError`.
- **Revoked key:** after `DELETE /admin/keys/{id}`, the same raw key must get 401, not 200.
- **Missing/blank bearer:** `Authorization: Bearer` with empty token, or no header, must 401 (not 500) when enabled.
- **Hash never leaks:** `GET /admin/keys` JSON must not contain `key_hash`, and the raw key must not appear in the list response.

---

### Task 1: Config — `AUTH_ENABLED`, `ADMIN_USER`, `ADMIN_PASS`

**Files:**
- Modify: `deepseek_proxy/settings.py` (Settings dataclass ~line 150-200, `__post_init__` ~line 210, module aliases ~line 280)
- Test: `tests/test_config_auth.py` (create)

**Interfaces:**
- Produces: `settings.auth_enabled: bool`, `settings.admin_user: str`, `settings.admin_pass: str`; module aliases `AUTH_ENABLED`, `ADMIN_USER`, `ADMIN_PASS`.

- [ ] **Step 1: Write the failing test**

```python
# tests/test_config_auth.py
import importlib.util
import os
from pathlib import Path
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
PACKAGE = ROOT / "deepseek_proxy"


def load_config(path, env=None):
    spec = importlib.util.spec_from_file_location("deepseek_proxy.settings", PACKAGE / "settings.py")
    module = importlib.util.module_from_spec(spec)
    with patch.dict(os.environ, {"DEEPSEEK_CONFIG": str(path), "DEEPSEEK_ENV_FILE": "", **(env or {})}, clear=True):
        with patch.dict(__import__("sys").modules, {spec.name: module}):
            spec.loader.exec_module(module)
    return module


def test_auth_defaults_off(tmp_path):
    cfg = load_config(tmp_path / "config.toml")
    assert cfg.AUTH_ENABLED is False
    assert cfg.ADMIN_USER == ""
    assert cfg.ADMIN_PASS == ""


def test_auth_enabled_requires_admin_creds(tmp_path):
    import pytest
    with pytest.raises(ValueError, match="ADMIN_USER and ADMIN_PASS"):
        load_config(tmp_path / "config.toml", {"AUTH_ENABLED": "true"})


def test_auth_enabled_with_creds_ok(tmp_path):
    cfg = load_config(tmp_path / "config.toml", {
        "AUTH_ENABLED": "true", "ADMIN_USER": "me", "ADMIN_PASS": "pw"
    })
    assert cfg.AUTH_ENABLED is True
    assert cfg.ADMIN_USER == "me"
```

- [ ] **Step 2: Run test to verify it fails**

Run: `.venv/bin/python -m pytest tests/test_config_auth.py -v`
Expected: FAIL — `AttributeError: module ... has no attribute 'AUTH_ENABLED'`.

- [ ] **Step 3: Add the settings fields and validation**

In `Settings`, near the other boolean flags:

```python
    auth_enabled: bool = field(default_factory=lambda: _to_bool(_env("AUTH_ENABLED", False)))
    admin_user: str = field(default_factory=lambda: str(_env("ADMIN_USER", "")))
    admin_pass: str = field(default_factory=lambda: str(_env("ADMIN_PASS", "")), repr=False)
```

In `__post_init__`, after the `storage_backend` checks:

```python
        if self.auth_enabled and (not self.admin_user or not self.admin_pass):
            raise ValueError("ADMIN_USER and ADMIN_PASS must be set when AUTH_ENABLED=true")
```

In the module-level aliases (near `STORAGE_BACKEND`):

```python
AUTH_ENABLED = settings.auth_enabled
ADMIN_USER = settings.admin_user
ADMIN_PASS = settings.admin_pass
```

- [ ] **Step 4: Run test to verify it passes**

Run: `.venv/bin/python -m pytest tests/test_config_auth.py -v`
Expected: PASS (3 passed).

- [ ] **Step 5: Commit**

```bash
git add deepseek_proxy/settings.py tests/test_config_auth.py
git commit -m "feat(auth): add AUTH_ENABLED/ADMIN_USER/ADMIN_PASS settings"
```

---

### Task 2: `auth.py` — pure helpers

**Files:**
- Create: `deepseek_proxy/auth.py`
- Test: `tests/test_auth.py` (create)

**Interfaces:**
- Produces:
  - `generate_key() -> str`
  - `hash_key(raw: str) -> str`
  - `check_admin(user: str, password: str, expected_user: str, expected_pass: str) -> bool`
  - `parse_basic(header: str | None) -> tuple[str, str] | None`
  - `parse_bearer(header: str | None) -> str | None`

- [ ] **Step 1: Write the failing test**

```python
# tests/test_auth.py
import base64
import hashlib

from deepseek_proxy.auth import (
    check_admin, generate_key, hash_key, parse_basic, parse_bearer,
)


def test_generate_key_format():
    k = generate_key()
    assert k.startswith("sk-")
    assert len(k) == 3 + 43
    assert generate_key() != generate_key()


def test_hash_key_deterministic():
    assert hash_key("sk-abc") == hashlib.sha256(b"sk-abc").hexdigest()


def test_check_admin():
    assert check_admin("me", "pw", "me", "pw") is True
    assert check_admin("me", "bad", "me", "pw") is False
    assert check_admin("", "", "me", "pw") is False


def test_parse_basic():
    tok = base64.b64encode(b"me:pw").decode()
    assert parse_basic(f"Basic {tok}") == ("me", "pw")
    assert parse_basic(None) is None
    assert parse_basic("Bearer x") is None
    assert parse_basic("Basic not-base64!!!") is None


def test_parse_bearer():
    assert parse_bearer("Bearer sk-abc") == "sk-abc"
    assert parse_bearer(None) is None
    assert parse_bearer("Bearer") is None
    assert parse_bearer("Basic zzz") is None
    assert parse_bearer("Bearer ") is None
```

- [ ] **Step 2: Run test to verify it fails**

Run: `.venv/bin/python -m pytest tests/test_auth.py -v`
Expected: FAIL — `ModuleNotFoundError: No module named 'deepseek_proxy.auth'`.

- [ ] **Step 3: Implement `deepseek_proxy/auth.py`**

Pure helpers only; FastAPI dependencies are added in Task 5.

```python
"""API-key generation and header parsing helpers."""
from __future__ import annotations

import base64
import binascii
import hashlib
import hmac
import secrets
from typing import Optional, Tuple


def generate_key() -> str:
    return "sk-" + secrets.token_urlsafe(32)


def hash_key(raw: str) -> str:
    return hashlib.sha256(raw.encode()).hexdigest()


def check_admin(user: str, password: str, expected_user: str, expected_pass: str) -> bool:
    return hmac.compare_digest(user, expected_user) and hmac.compare_digest(password, expected_pass)


def parse_basic(header: Optional[str]) -> Optional[Tuple[str, str]]:
    if not header or not header.startswith("Basic "):
        return None
    try:
        decoded = base64.b64decode(header[6:]).decode("utf-8")
    except (binascii.Error, UnicodeDecodeError, ValueError):
        return None
    if ":" not in decoded:
        return None
    user, password = decoded.split(":", 1)
    return user, password


def parse_bearer(header: Optional[str]) -> Optional[str]:
    if not header or not header.startswith("Bearer "):
        return None
    token = header[7:].strip()
    return token or None
```

- [ ] **Step 4: Run test to verify it passes**

Run: `.venv/bin/python -m pytest tests/test_auth.py -v`
Expected: PASS (5 passed).

- [ ] **Step 5: Commit**

```bash
git add deepseek_proxy/auth.py tests/test_auth.py
git commit -m "feat(auth): key generation and header parsing helpers"
```

---

### Task 3: `api_keys` persistence — `SqliteBackend`

**Files:**
- Modify: `deepseek_proxy/storage.py` (`StorageBackend` protocol ~line 46; `_SQLITE_SCHEMA` ~line 69; `SqliteBackend` ~line 90)
- Test: `tests/test_storage_api_keys.py` (create)

**Interfaces:**
- Consumes: nothing.
- Produces on `SqliteBackend`: `insert_api_key(record: dict) -> None`, `list_api_keys() -> list[dict]`, `find_api_key_by_hash(key_hash: str) -> dict | None`, `delete_api_key(key_id: str) -> bool`, `touch_api_key_used(key_id: str, when: float) -> None`.
- Record dict keys: `id`, `key_hash`, `key_prefix`, `name`, `created_at`, `last_used_at`.
- `list_api_keys` returns dicts WITHOUT `key_hash`, newest first.

- [ ] **Step 1: Write the failing test**

```python
# tests/test_storage_api_keys.py
import tempfile
from pathlib import Path

from deepseek_proxy.storage import SqliteBackend


def _backend():
    d = tempfile.mkdtemp()
    b = SqliteBackend(str(Path(d) / "k.db"))
    b.open()
    return b


def test_insert_find_delete_touch():
    b = _backend()
    rec = {"id": "id1", "key_hash": "h1", "key_prefix": "sk-a", "name": "first",
           "created_at": 1.0, "last_used_at": None}
    b.insert_api_key(rec)
    assert b.find_api_key_by_hash("h1")["name"] == "first"
    b.touch_api_key_used("id1", 2.0)
    assert b.find_api_key_by_hash("h1")["last_used_at"] == 2.0
    assert b.delete_api_key("id1") is True
    assert b.find_api_key_by_hash("h1") is None
    assert b.delete_api_key("id1") is False
    b.close()


def test_list_excludes_hash_and_is_newest_first():
    b = _backend()
    b.insert_api_key({"id": "a", "key_hash": "ha", "key_prefix": "sk-a", "name": "a",
                      "created_at": 1.0, "last_used_at": None})
    b.insert_api_key({"id": "b", "key_hash": "hb", "key_prefix": "sk-b", "name": "b",
                      "created_at": 2.0, "last_used_at": None})
    rows = b.list_api_keys()
    assert [r["id"] for r in rows] == ["b", "a"]
    assert all("key_hash" not in r for r in rows)
    b.close()
```

- [ ] **Step 2: Run test to verify it fails**

Run: `.venv/bin/python -m pytest tests/test_storage_api_keys.py -v`
Expected: FAIL — `AttributeError: 'SqliteBackend' object has no attribute 'insert_api_key'`.

- [ ] **Step 3: Add the table and methods**

Append to `_SQLITE_SCHEMA`:

```sql
CREATE TABLE IF NOT EXISTS api_keys (
    id            TEXT PRIMARY KEY,
    key_hash      TEXT NOT NULL UNIQUE,
    key_prefix    TEXT NOT NULL,
    name          TEXT NOT NULL,
    created_at    REAL NOT NULL,
    last_used_at  REAL
);
```

Add the five methods to `SqliteBackend` (use `self._lock` and `self._conn` like existing methods; `list_api_keys` selects columns explicitly, omitting `key_hash`, ordered `created_at DESC`). Add the same five method signatures to the `StorageBackend` Protocol (bodies `...`).

- [ ] **Step 4: Run test to verify it passes**

Run: `.venv/bin/python -m pytest tests/test_storage_api_keys.py -v`
Expected: PASS (2 passed).

- [ ] **Step 5: Commit**

```bash
git add deepseek_proxy/storage.py tests/test_storage_api_keys.py
git commit -m "feat(auth): api_keys persistence on SqliteBackend"
```

---

### Task 4: `api_keys` persistence — `MongoBackend`

**Files:**
- Modify: `deepseek_proxy/storage.py` (`MongoBackend` ~line 187)
- Test: `tests/test_storage_mongo.py` (extend the existing skip-guarded file)

**Interfaces:**
- Produces the same five methods on `MongoBackend`, same record shape, same `list_api_keys` contract (no `key_hash`, newest first).

- [ ] **Step 1: Write the failing test**

Append to `tests/test_storage_mongo.py` (inside the skip-guarded class):

```python
    def test_api_keys_roundtrip(self):
        b = self.store._backend  # MongoBackend from setUp
        b.insert_api_key({"id": "m1", "key_hash": "mh1", "key_prefix": "sk-m",
                          "name": "mongo", "created_at": 1.0, "last_used_at": None})
        assert b.find_api_key_by_hash("mh1")["name"] == "mongo"
        b.touch_api_key_used("m1", 3.0)
        assert b.find_api_key_by_hash("mh1")["last_used_at"] == 3.0
        rows = b.list_api_keys()
        assert any(r["id"] == "m1" for r in rows)
        assert all("key_hash" not in r for r in rows)
        assert b.delete_api_key("m1") is True
        assert b.find_api_key_by_hash("mh1") is None
```

- [ ] **Step 2: Run test to verify it fails**

Run: `.venv/bin/python -m pytest tests/test_storage_mongo.py -v`
Expected: SKIP without `MONGO_TEST_URI`; with it, FAIL — `AttributeError: 'MongoBackend' object has no attribute 'insert_api_key'`.

- [ ] **Step 3: Implement on `MongoBackend`**

In `open()`, bind `self._api_keys = db["api_keys"]` and ensure a unique index: `self._api_keys.create_index("key_hash", unique=True)`. Implement the five methods with pymongo (`insert_one`, `find_one`, `delete_one`, `update_one`, `find` with projection `{"key_hash": 0}` sorted `created_at DESC`). Store `id` as a normal field, not `_id`; use `_id=id` only if you prefer — the lookup is by `key_hash`, so keep `id` a plain field for simplicity.

- [ ] **Step 4: Run test to verify it passes**

Run: `MONGO_TEST_URI=... .venv/bin/python -m pytest tests/test_storage_mongo.py -v`
Expected: PASS. Without the URI: SKIP (unchanged).

- [ ] **Step 5: Commit**

```bash
git add deepseek_proxy/storage.py tests/test_storage_mongo.py
git commit -m "feat(auth): api_keys persistence on MongoBackend"
```

---

### Task 5: FastAPI dependencies (`require_admin`, `require_api_key`)

**Files:**
- Modify: `deepseek_proxy/auth.py` (append FastAPI deps)
- Test: `tests/test_auth_deps.py` (create)

**Interfaces:**
- Consumes: helpers from Task 2; `Settings` from Task 1.
- Produces:
  - `require_admin(request: Request) -> None` — raises `HTTPException(401, headers={"WWW-Authenticate": "Basic"})` on bad/missing creds.
  - `require_api_key(request: Request) -> None` — no-op if `request.app.state.settings.auth_enabled` is false; else 401 on missing/unknown key.
- Both read `request.app.state.settings` and `request.app.state.store` (the `StateStore`).

- [ ] **Step 1: Write the failing test**

```python
# tests/test_auth_deps.py
from fastapi import FastAPI, Depends
from fastapi.testclient import TestClient

from deepseek_proxy.auth import require_admin, require_api_key


class _FakeSettings:
    auth_enabled = True
    admin_user = "me"
    admin_pass = "pw"


class _FakeBackend:
    def __init__(self): self.rows = {}
    def find_api_key_by_hash(self, h): return self.rows.get(h)
    def touch_api_key_used(self, i, w): pass


class _FakeStore:
    def __init__(self): self._backend = _FakeBackend()


def _app():
    app = FastAPI()
    app.state.settings = _FakeSettings()
    app.state.store = _FakeStore()

    @app.get("/v1/x", dependencies=[Depends(require_api_key)])
    def x(): return {"ok": True}

    @app.get("/admin/x", dependencies=[Depends(require_admin)])
    def ax(): return {"ok": True}
    return app


import base64


def test_api_key_required_and_valid():
    c = TestClient(_app())
    assert c.get("/v1/x").status_code == 401
    from deepseek_proxy.auth import hash_key
    c.app.state.store._backend.rows[hash_key("sk-ok")] = {"id": "1"}
    assert c.get("/v1/x", headers={"Authorization": "Bearer sk-ok"}).status_code == 200
    assert c.get("/v1/x", headers={"Authorization": "Bearer sk-bad"}).status_code == 401


def test_admin_basic():
    c = TestClient(_app())
    assert c.get("/admin/x").status_code == 401
    tok = base64.b64encode(b"me:pw").decode()
    assert c.get("/admin/x", headers={"Authorization": f"Basic {tok}"}).status_code == 200


def test_api_key_noop_when_disabled():
    c = TestClient(_app())
    c.app.state.settings.auth_enabled = False
    assert c.get("/v1/x").status_code == 200
```

- [ ] **Step 2: Run test to verify it fails**

Run: `.venv/bin/python -m pytest tests/test_auth_deps.py -v`
Expected: FAIL — `ImportError: cannot import name 'require_admin'`.

- [ ] **Step 3: Implement the dependencies in `auth.py`**

```python
from fastapi import HTTPException, Request


def require_admin(request: Request) -> None:
    settings = request.app.state.settings
    creds = parse_basic(request.headers.get("authorization"))
    if creds is None or not check_admin(creds[0], creds[1], settings.admin_user, settings.admin_pass):
        raise HTTPException(status_code=401, detail="invalid admin credentials",
                            headers={"WWW-Authenticate": "Basic"})


def require_api_key(request: Request) -> None:
    settings = request.app.state.settings
    if not settings.auth_enabled:
        return
    token = parse_bearer(request.headers.get("authorization"))
    if token is None:
        raise HTTPException(status_code=401, detail="missing api key")
    row = request.app.state.store._backend.find_api_key_by_hash(hash_key(token))
    if row is None:
        raise HTTPException(status_code=401, detail="invalid api key")
    try:
        request.app.state.store._backend.touch_api_key_used(row["id"], __import__("time").time())
    except Exception:
        pass
```

- [ ] **Step 4: Run test to verify it passes**

Run: `.venv/bin/python -m pytest tests/test_auth_deps.py -v`
Expected: PASS (3 passed).

- [ ] **Step 5: Commit**

```bash
git add deepseek_proxy/auth.py tests/test_auth_deps.py
git commit -m "feat(auth): require_admin and require_api_key dependencies"
```

---

### Task 6: Admin UI module

**Files:**
- Create: `deepseek_proxy/admin_ui.py`
- Test: `tests/test_admin_ui.py` (create)

**Interfaces:**
- Produces: `ADMIN_HTML: str` — a complete self-contained HTML document.

- [ ] **Step 1: Write the failing test**

```python
# tests/test_admin_ui.py
from deepseek_proxy.admin_ui import ADMIN_HTML


def test_admin_html_self_contained():
    assert ADMIN_HTML.strip().startswith("<!DOCTYPE html>")
    assert "<script" in ADMIN_HTML
    assert "/admin/keys" in ADMIN_HTML
    # no external assets
    assert "http://" not in ADMIN_HTML
    assert "https://" not in ADMIN_HTML
```

- [ ] **Step 2: Run test to verify it fails**

Run: `.venv/bin/python -m pytest tests/test_admin_ui.py -v`
Expected: FAIL — `ModuleNotFoundError`.

- [ ] **Step 3: Implement `deepseek_proxy/admin_ui.py`**

A single triple-quoted `ADMIN_HTML` string: minimal styling, a table populated by `fetch('/admin/keys')`, a create form that `POST`s `{name}` and shows the returned `key` once in a copyable `<code>` block, and a per-row Revoke button that `DELETE`s `/admin/keys/{id}`. No external fonts/scripts. Keep it under ~120 lines.

- [ ] **Step 4: Run test to verify it passes**

Run: `.venv/bin/python -m pytest tests/test_admin_ui.py -v`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add deepseek_proxy/admin_ui.py tests/test_admin_ui.py
git commit -m "feat(auth): self-contained admin management UI"
```

---

### Task 7: Wire routes into `app.py`

**Files:**
- Modify: `deepseek_proxy/app.py` (imports ~line 8-19; `create_app` ~line 24; routes ~line 129-161; lifespan sets `app.state.store`/`app.state.settings` ~line 38-85)
- Test: `tests/test_app_auth.py` (create)

**Interfaces:**
- Consumes: `require_admin`, `require_api_key`, `generate_key`, `hash_key` (Tasks 2,5); `ADMIN_HTML` (Task 6); storage methods (Tasks 3,4).
- Produces: HTTP routes listed in the spec; `app.state.settings` and `app.state.store` set during lifespan.

- [ ] **Step 1: Write the failing test**

```python
# tests/test_app_auth.py
import uuid
from fastapi.testclient import TestClient

from deepseek_proxy.app import create_app


def _settings(**over):
    from deepseek_proxy.settings import settings as s
    import dataclasses
    return dataclasses.replace(s, **over)


def _client(auth_enabled, admin_user="me", admin_pass="pw"):
    app = create_app(settings=_settings(auth_enabled=auth_enabled, admin_user=admin_user, admin_pass=admin_pass))
    return TestClient(app)


def test_local_default_is_open_and_no_admin(tmp_path, monkeypatch):
    # use a temp sqlite db so we don't touch the real one
    monkeypatch.chdir(tmp_path)
    c = _client(auth_enabled=False)
    with c:
        assert c.get("/v1/models").status_code == 200
        assert c.get("/admin").status_code == 404


def test_enabled_requires_key_and_admin(tmp_path, monkeypatch):
    import base64
    monkeypatch.chdir(tmp_path)
    c = _client(auth_enabled=True)
    with c:
        assert c.get("/v1/models").status_code == 401
        tok = base64.b64encode(b"me:pw").decode()
        admin = {"Authorization": f"Basic {tok}"}
        assert c.get("/admin", headers=admin).status_code == 200
        created = c.post("/admin/keys", json={"name": "t"}, headers=admin)
        assert created.status_code == 200
        key = created.json()["key"]
        assert c.get("/v1/models", headers={"Authorization": f"Bearer {key}"}).status_code == 200
        kid = created.json()["id"]
        assert c.delete(f"/admin/keys/{kid}", headers=admin).status_code == 200
        assert c.get("/v1/models", headers={"Authorization": f"Bearer {key}"}).status_code == 401
```

- [ ] **Step 2: Run test to verify it fails**

Run: `.venv/bin/python -m pytest tests/test_app_auth.py -v`
Expected: FAIL — `/admin` returns 404 even when enabled (routes not added yet) and `/v1/models` returns 200 when enabled (no dependency yet).

- [ ] **Step 3: Wire it up in `app.py`**

1. Import `require_admin`, `require_api_key`, `generate_key`, `hash_key` from `.auth`; `ADMIN_HTML` from `.admin_ui`.
2. In `create_app`, set `app.state.settings = settings` before returning.
3. In the lifespan (where `store` is created), also set `app.state.store = store` (and ensure it is set even when `pool` is passed in — set it whenever `store` exists).
4. Attach `dependencies=[Depends(require_api_key)]` to `completions` and `models` routes.
5. After the existing routes, conditionally register admin routes when `settings.auth_enabled`:

```python
    if settings.auth_enabled:
        from fastapi import Depends

        @app.get("/admin", dependencies=[Depends(require_admin)])
        async def admin_page():
            from fastapi.responses import HTMLResponse
            return HTMLResponse(ADMIN_HTML)

        @app.get("/admin/keys", dependencies=[Depends(require_admin)])
        async def list_keys():
            return {"keys": app.state.store._backend.list_api_keys()}

        @app.post("/admin/keys", dependencies=[Depends(require_admin)])
        async def create_key(request: Request):
            body = await request.json()
            name = (body or {}).get("name", "").strip()
            if not name:
                raise HTTPException(status_code=400, detail="name is required")
            raw = generate_key()
            import time as _t, uuid as _u
            rec = {"id": _u.uuid4().hex, "key_hash": hash_key(raw), "key_prefix": raw[:7],
                   "name": name, "created_at": _t.time(), "last_used_at": None}
            app.state.store._backend.insert_api_key(rec)
            return {k: rec[k] for k in ("id", "name", "key_prefix", "created_at")} | {"key": raw}

        @app.delete("/admin/keys/{key_id}", dependencies=[Depends(require_admin)])
        async def delete_key(key_id: str):
            if not app.state.store._backend.delete_api_key(key_id):
                raise HTTPException(status_code=404, detail="not found")
            return {"deleted": key_id}
```

Add `from fastapi import HTTPException` to the imports.

- [ ] **Step 4: Run test to verify it passes**

Run: `.venv/bin/python -m pytest tests/test_app_auth.py -v`
Expected: PASS (2 passed).

- [ ] **Step 5: Run the full suite and lint**

Run: `.venv/bin/python -m pytest tests/ -q && .venv/bin/ruff check .`
Expected: all pass; lint clean.

- [ ] **Step 6: Commit**

```bash
git add deepseek_proxy/app.py tests/test_app_auth.py
git commit -m "feat(auth): gate /v1 with keys and mount admin routes when enabled"
```

---

### Task 8: Deployment config + docs

**Files:**
- Modify: `render.yaml`, `.env.example`, `config.example.toml`

**Interfaces:**
- Produces: deployment env vars and example docs.

- [ ] **Step 1: Add to `render.yaml`** (under `envVars`)

```yaml
      - key: AUTH_ENABLED
        value: "true"
      - key: ADMIN_USER
        sync: false
      - key: ADMIN_PASS
        sync: false
```

- [ ] **Step 2: Add to `.env.example`**

```
# --- API key auth (off locally by default) ---
# Set AUTH_ENABLED=true and provide ADMIN_USER/ADMIN_PASS to require keys on /v1/*
# and expose the /admin management UI. Leave false for local development.
AUTH_ENABLED=false
ADMIN_USER=
ADMIN_PASS=
```

- [ ] **Step 3: Add to `config.example.toml`** (non-secret keys only)

```toml
# --- API key auth (secrets ADMIN_USER/ADMIN_PASS live in .env) ---
AUTH_ENABLED = false
```

- [ ] **Step 4: Verify tests still green**

Run: `.venv/bin/python -m pytest tests/ -q && .venv/bin/ruff check .`
Expected: pass, lint clean.

- [ ] **Step 5: Commit**

```bash
git add render.yaml .env.example config.example.toml docs/superpowers/plans/2026-10-08-api-key-auth.md
git commit -m "docs/deploy: AUTH_ENABLED + ADMIN_USER/ADMIN_PASS"
```

---

## Self-Review

**Spec coverage:** config (T1), key gen/hash/parse (T2), persistence SQLite (T3) + Mongo (T4), dependencies (T5), UI (T6), routes/wiring (T7), deploy docs (T8). All spec sections covered.

**Type consistency:** `insert_api_key`/`list_api_keys`/`find_api_key_by_hash`/`delete_api_key`/`touch_api_key_used` are named identically in T3, T4, T5, T7. `auth_enabled`/`admin_user`/`admin_pass` consistent across T1, T5, T7.

**Review Focus tests:** disabled-open + `/admin` 404 (T7 Step 1 first test); enabled-without-creds ValueError (T1); revoked-key 401 (T7 Step 1 second test); blank bearer 401 (T5); hash not leaked (T3, T4 list tests).
