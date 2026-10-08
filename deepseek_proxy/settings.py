"""Configuration loading.

Two files, two jobs:
    .env         secrets and per-environment settings (git-ignored)
    config.toml  non-secret application behavior (git-ignored; see
                 config.example.toml for the committed template)

Precedence (highest wins):
    1. Process environment variables (override without editing a file).
    2. Values from .env (loaded into the environment below).
    3. Values from config.toml (stdlib tomllib).
    4. Built-in defaults.

.env is loaded first so it populates os.environ before any settings read;
actual environment variables still win over .env, and both win over TOML.
Kept module-level names for backwards compatibility.
"""

from __future__ import annotations

import os
import math
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Dict, List, Optional

import tomllib

ROOT = Path.cwd()

# Load .env before anything reads os.environ. override=False preserves real
# environment variables (e.g. those set by Render/systemd) over .env entries.
#
# DEEPSEEK_ENV_FILE overrides the path; set it to the empty string to skip
# loading entirely (tests do this so a developer's real .env cannot leak into
# an intentionally clean environment).
_env_file = os.getenv("DEEPSEEK_ENV_FILE", str(ROOT / ".env"))
if _env_file:
    try:
        from dotenv import load_dotenv

        load_dotenv(_env_file, override=False)
    except ImportError:  # pragma: no cover - python-dotenv is a declared dependency
        pass

CONFIG_PATH = Path(os.getenv("DEEPSEEK_CONFIG", str(ROOT / "config.toml")))


def _load_toml(path: Path) -> Dict[str, Any]:
    try:
        with open(path, "rb") as f:
            return tomllib.load(f)
    except FileNotFoundError:
        return {}
    except tomllib.TOMLDecodeError as exc:
        raise ValueError(f"Invalid TOML configuration in {path}") from exc


_TOML = _load_toml(CONFIG_PATH)


def _env(name: str, default: Any) -> Any:
    if name in os.environ:
        return os.environ[name]
    if name in _TOML:
        return _TOML[name]
    return default


def _int(name: str, default: int) -> int:
    value = _env(name, default)
    if isinstance(value, bool) or not isinstance(value, (int, str)):
        raise ValueError(f"{name} must be an integer")
    try:
        return int(value)
    except ValueError:
        raise ValueError(f"{name} must be an integer") from None


def _float(name: str, default: float) -> float:
    value = _env(name, default)
    if isinstance(value, bool):
        raise ValueError(f"{name} must be a number")
    try:
        return float(value)
    except (ValueError, TypeError):
        raise ValueError(f"{name} must be a number") from None


def _to_bool(v: Any) -> bool:
    """Coerce a config value to bool: bools pass through, strings are checked."""
    if isinstance(v, bool):
        return v
    return str(v).strip().lower() in ("1", "true", "yes", "on")


def _load_accounts(environ, toml: Dict[str, Any]) -> List[Dict[str, str]]:
    """Build the account list from indexed email/password env vars.

    Scans indexed credentials, then falls back to a single account. Token-only
    accounts use a stable, non-secret routing identifier.
    """

    def value(name):
        return environ[name] if name in environ else toml.get(name)

    accounts: List[Dict[str, str]] = []
    i = 0
    while True:
        email = value(f"DEEPSEEK_EMAIL_{i}")
        password = value(f"DEEPSEEK_PASSWORD_{i}")
        token = value(f"DEEPSEEK_TOKEN_{i}")
        if not token and not (email and password):
            break
        account = {"email": email or f"token-account-{i}", "password": password or ""}
        if token:
            account["token"] = token
        accounts.append(account)
        i += 1
    if accounts:
        return accounts
    email = value("DEEPSEEK_EMAIL")
    password = value("DEEPSEEK_PASSWORD")
    token = value("DEEPSEEK_TOKEN")
    if token or (email and password):
        account = {"email": email or "token-account", "password": password or ""}
        if token:
            account["token"] = token
        return [account]
    return []


@dataclass(frozen=True)
class Settings:
    deepseek_token: Optional[str] = field(
        default_factory=lambda: _env("DEEPSEEK_TOKEN", None), repr=False
    )
    deepseek_email: Optional[str] = field(default_factory=lambda: _env("DEEPSEEK_EMAIL", None))
    deepseek_password: Optional[str] = field(
        default_factory=lambda: _env("DEEPSEEK_PASSWORD", None), repr=False
    )
    model_type: str = field(default_factory=lambda: _env("MODEL_TYPE", "DEFAULT").upper())
    accounts: List[Dict[str, str]] = field(
        default_factory=lambda: _load_accounts(os.environ, _TOML), repr=False
    )
    proxy_host: str = field(default_factory=lambda: _env("PROXY_HOST", "0.0.0.0"))
    proxy_port: int = field(default_factory=lambda: _int("PROXY_PORT", 8000))
    request_delay: float = field(default_factory=lambda: _float("REQUEST_DELAY", 2.0))
    queue_limit: int = field(default_factory=lambda: _int("QUEUE_LIMIT", 64))
    queue_timeout: float = field(default_factory=lambda: _float("QUEUE_TIMEOUT", 120.0))
    request_timeout: float = field(default_factory=lambda: _float("REQUEST_TIMEOUT", 300.0))
    heartbeat_interval: float = field(default_factory=lambda: _float("HEARTBEAT_INTERVAL", 10.0))
    image_max_bytes: int = field(default_factory=lambda: _int("IMAGE_MAX_BYTES", 10 * 1024 * 1024))
    tool_reminder_interval: int = field(default_factory=lambda: _int("TOOL_REMINDER_INTERVAL", 0))
    system_prompt_interval: int = field(default_factory=lambda: _int("SYSTEM_PROMPT_INTERVAL", 0))
    tool_buffer_limit: int = field(default_factory=lambda: _int("TOOL_BUFFER_LIMIT", 100000))
    tool_repair_max_retries: int = field(default_factory=lambda: _int("TOOL_REPAIR_MAX_RETRIES", 3))
    rate_limit_max_retries: int = field(default_factory=lambda: _int("RATE_LIMIT_MAX_RETRIES", 3))
    rate_limit_backoff_s: float = field(
        default_factory=lambda: _float("RATE_LIMIT_BACKOFF_S", 10.0)
    )
    tool_format: str = field(default_factory=lambda: str(_env("TOOL_FORMAT", "json_invoke")))
    title_prompt_marker: str = "You name chat sessions"
    thread_prefix: str = "thread_"
    title_prefix: str = "title_"
    thread_hash_len: int = 24
    models: List[Dict[str, Any]] = field(
        default_factory=lambda: list(
            _TOML.get(
                "MODELS",
                [
                    {"id": name, "object": "model", "created": 0, "owned_by": "deepseek"}
                    for name in ("DEFAULT", "EXPERT", "VISION")
                ],
            )
        )
    )
    state_path: Path = field(
        default_factory=lambda: Path(_env("STATE_PATH", str(ROOT / "session_state.json")))
    )
    db_path: Path = field(default_factory=lambda: Path(_env("DB_PATH", str(ROOT / "dispatch.db"))))
    storage_backend: str = field(
        default_factory=lambda: str(_env("STORAGE_BACKEND", "sqlite")).strip().lower()
    )
    auth_enabled: bool = field(default_factory=lambda: _to_bool(_env("AUTH_ENABLED", False)))
    admin_user: str = field(default_factory=lambda: str(_env("ADMIN_USER", "")))
    admin_pass: str = field(default_factory=lambda: str(_env("ADMIN_PASS", "")), repr=False)
    mongodb_uri: str = field(default_factory=lambda: str(_env("MONGODB_URI", "")), repr=False)
    mongodb_db: str = field(default_factory=lambda: str(_env("MONGODB_DB", "deepseek_proxy")))
    idle_timeout: float = field(default_factory=lambda: _float("IDLE_TIMEOUT", 300.0))
    prompts: Dict[str, Any] = field(default_factory=lambda: dict(_TOML.get("PROMPTS", {})))
    log_level: str = field(default_factory=lambda: _env("LOG_LEVEL", "INFO").upper())
    log_format: str = field(default_factory=lambda: _env("LOG_FORMAT", "pretty").lower())
    debug: bool = field(default_factory=lambda: _to_bool(_env("DEBUG", False)))
    thinking_enabled: bool = field(
        default_factory=lambda: _to_bool(_env("THINKING_ENABLED", False))
    )
    search_enabled: bool = field(
        default_factory=lambda: _to_bool(_env("SEARCH_ENABLED", True))
    )

    def __post_init__(self) -> None:
        integer_minima = {
            "proxy_port": 1,
            "queue_limit": 1,
            "image_max_bytes": 1,
            "tool_reminder_interval": 0,
            "system_prompt_interval": 0,
            "tool_buffer_limit": 1,
            "tool_repair_max_retries": 0,
            "rate_limit_max_retries": 0,
            "thread_hash_len": 1,
        }
        for name, minimum in integer_minima.items():
            value = getattr(self, name)
            if isinstance(value, bool) or not isinstance(value, int) or value < minimum:
                raise ValueError(f"{name.upper()} must be an integer >= {minimum}")
        if self.proxy_port > 65535:
            raise ValueError("PROXY_PORT must be <= 65535")
        for name in (
            "request_delay",
            "queue_timeout",
            "request_timeout",
            "heartbeat_interval",
            "idle_timeout",
            "rate_limit_backoff_s",
        ):
            value = getattr(self, name)
            allow_zero = name == "request_delay"
            if (
                isinstance(value, bool)
                or not isinstance(value, (float, int))
                or not math.isfinite(value)
                or value < 0
                or (value == 0 and not allow_zero)
            ):
                bound = ">= 0" if allow_zero else "> 0"
                raise ValueError(f"{name.upper()} must be finite and {bound}")
        if self.log_level not in (
            "DEBUG",
            "INFO",
            "WARNING",
            "WARN",
            "ERROR",
            "CRITICAL",
            "FATAL",
            "NOTSET",
        ):
            raise ValueError("LOG_LEVEL is not a recognized logging level")
        if self.log_format not in ("pretty", "json"):
            raise ValueError("LOG_FORMAT must be pretty or json")
        if self.storage_backend not in ("sqlite", "mongo"):
            raise ValueError("STORAGE_BACKEND must be sqlite or mongo")
        if self.storage_backend == "mongo" and not self.mongodb_uri:
            raise ValueError("MONGODB_URI must be set when STORAGE_BACKEND=mongo")
        if self.auth_enabled and (not self.admin_user or not self.admin_pass):
            raise ValueError("ADMIN_USER and ADMIN_PASS must be set when AUTH_ENABLED=true")

    @property
    def tool_call_template(self) -> str:
        from .tools.format import ACTIVE

        return ACTIVE.template()


settings = Settings()

DEEPSEEK_TOKEN = settings.deepseek_token
DEEPSEEK_EMAIL = settings.deepseek_email
DEEPSEEK_PASSWORD = settings.deepseek_password
MODEL_TYPE = settings.model_type
THINKING_ENABLED = settings.thinking_enabled
SEARCH_ENABLED = settings.search_enabled
ACCOUNTS = settings.accounts
PROXY_HOST = settings.proxy_host
PROXY_PORT = settings.proxy_port
REQUEST_DELAY = settings.request_delay
QUEUE_LIMIT = settings.queue_limit
QUEUE_TIMEOUT = settings.queue_timeout
REQUEST_TIMEOUT = settings.request_timeout
HEARTBEAT_INTERVAL = settings.heartbeat_interval
IMAGE_MAX_BYTES = settings.image_max_bytes
TOOL_FORMAT = settings.tool_format
TOOL_REMINDER_INTERVAL = settings.tool_reminder_interval
SYSTEM_PROMPT_INTERVAL = settings.system_prompt_interval
TOOL_BUFFER_LIMIT = settings.tool_buffer_limit
TOOL_REPAIR_MAX_RETRIES = settings.tool_repair_max_retries
RATE_LIMIT_MAX_RETRIES = settings.rate_limit_max_retries
RATE_LIMIT_BACKOFF_S = settings.rate_limit_backoff_s
# Deprecated aliases — the wire format lives in tool_format.py (ACTIVE dialect).
# Kept so old imports don't break; new code must import from tool_format.
_TOOL_ALIASES = {
    "TOOL_CALL_PREFIX",
    "TOOL_CALL_SUFFIX",
    "TOOL_CALL_TEMPLATE",
    "TOOL_PARAM_CLOSE",
    "TOOL_PARAM_OPEN",
    "TOOL_TAG_CLOSE",
    "TOOL_TAG_OPEN",
    "TOOL_WRAPPER_CLOSE",
    "TOOL_WRAPPER_OPEN",
}


def __getattr__(name: str) -> Any:
    if name in _TOOL_ALIASES:
        from .tools import format as tool_format

        return getattr(tool_format, name)
    raise AttributeError(f"module {__name__!r} has no attribute {name!r}")


def __dir__() -> List[str]:
    return sorted(set(globals()) | _TOOL_ALIASES)


MODELS = settings.models
STATE_PATH = settings.state_path
DB_PATH = settings.db_path
STORAGE_BACKEND = settings.storage_backend
MONGODB_URI = settings.mongodb_uri
MONGODB_DB = settings.mongodb_db
AUTH_ENABLED = settings.auth_enabled
ADMIN_USER = settings.admin_user
ADMIN_PASS = settings.admin_pass
IDLE_TIMEOUT = settings.idle_timeout

_enc = None


def estimate_tokens(text: str) -> int:
    global _enc
    if not text:
        return 0
    if _enc is None:
        import tiktoken

        _enc = tiktoken.get_encoding("cl100k_base")
    return len(_enc.encode(text, disallowed_special=()))


def render_prompt(template: str, **ctx: Any) -> str:
    """Substitute {placeholders} in a prompt template, including the real
    tool-call tag strings (resolved from Settings). Tag tokens stored as
    TOOL_CALL_OPEN/CLOSE/TOOL_PARAM_OPEN/CLOSE in the config file are replaced
    with their actual values."""
    resolved = dict(ctx)
    from .tools.format import ACTIVE

    resolved.setdefault("tool_call_template", ACTIVE.template())
    resolved.setdefault("tools_block", "")
    out = template
    # Replace placeholder token names in prompt prose with the ACTIVE dialect tags.
    from .tools.format import _placeholder_map

    for token, value in _placeholder_map().items():
        out = out.replace(token, value)
    return out.format(**resolved)


__all__ = sorted({name for name in globals() if not name.startswith("_")} | _TOOL_ALIASES)
