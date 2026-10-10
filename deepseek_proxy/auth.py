"""API-key generation and header parsing helpers."""

from __future__ import annotations

import base64
import binascii
import hashlib
import hmac
import secrets
import time
from typing import Optional, Tuple

from fastapi import HTTPException, Request


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


def require_admin(request: Request) -> None:
    settings = request.app.state.settings
    creds = parse_basic(request.headers.get("authorization"))
    if creds is None or not check_admin(
        creds[0], creds[1], settings.admin_user, settings.admin_pass
    ):
        raise HTTPException(
            status_code=401,
            detail="invalid admin credentials",
            headers={"WWW-Authenticate": "Basic"},
        )


def require_api_key(request: Request) -> None:
    settings = getattr(request.app.state, "settings", None)
    # Default to NOT enforcing when settings are absent/partial: preserves the
    # pre-auth behavior for local runs and minimal test doubles.
    if not getattr(settings, "auth_enabled", False):
        return
    token = parse_bearer(request.headers.get("authorization"))
    if token is None:
        raise HTTPException(status_code=401, detail="missing api key")
    backend = request.app.state.store._backend
    row = backend.find_api_key_by_hash(hash_key(token))
    if row is None:
        raise HTTPException(status_code=401, detail="invalid api key")
    try:
        backend.touch_api_key_used(row["id"], time.time())
    except Exception:
        pass
