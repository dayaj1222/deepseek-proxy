"""Configuration loading: config.toml is the source of truth.

Precedence (highest wins):
    1. Process environment variables (override without editing the file).
    2. Values from config.toml (stdlib tomllib).
    3. Built-in defaults.

No .env layer. Kept module-level names for backwards compatibility.
"""

from __future__ import annotations

import os
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Dict, List, Optional

import tiktoken
import tomllib

ROOT = Path(__file__).resolve().parent
CONFIG_PATH = Path(os.getenv("DEEPSEEK_CONFIG", str(ROOT / "config.toml")))


def _load_toml(path: Path) -> Dict[str, Any]:
    try:
        with open(path, "rb") as f:
            return tomllib.load(f)
    except (FileNotFoundError, OSError, tomllib.TOMLDecodeError):
        return {}


_TOML = _load_toml(CONFIG_PATH)


def _env(name: str, default: Any) -> Any:
    if name in os.environ:
        return os.environ[name]
    if name in _TOML:
        return _TOML[name]
    return default


def _int(name: str, default: int) -> int:
    return int(_env(name, default))


def _float(name: str, default: float) -> float:
    return float(_env(name, default))


def _to_bool(v: Any) -> bool:
    """Coerce a config value to bool: bools pass through, strings are checked."""
    if isinstance(v, bool):
        return v
    return str(v).strip().lower() in ("1", "true", "yes", "on")


@dataclass(frozen=True)
class Settings:
    deepseek_token: Optional[str] = field(
        default_factory=lambda: (
            os.getenv("DEEPSEEK_TOKEN") or _TOML.get("DEEPSEEK_TOKEN")
        )
    )
    deepseek_email: Optional[str] = field(
        default_factory=lambda: (
            os.getenv("DEEPSEEK_EMAIL") or _TOML.get("DEEPSEEK_EMAIL")
        )
    )
    deepseek_password: Optional[str] = field(
        default_factory=lambda: (
            os.getenv("DEEPSEEK_PASSWORD") or _TOML.get("DEEPSEEK_PASSWORD")
        )
    )
    model_type: str = field(
        default_factory=lambda: _env("MODEL_TYPE", "DEFAULT").upper()
    )
    proxy_host: str = field(default_factory=lambda: _env("PROXY_HOST", "0.0.0.0"))
    proxy_port: int = field(default_factory=lambda: _int("PROXY_PORT", 8000))
    request_delay: float = field(default_factory=lambda: _float("REQUEST_DELAY", 0.0))
    tool_reminder_interval: int = field(
        default_factory=lambda: _int("TOOL_REMINDER_INTERVAL", 0)
    )
    tool_buffer_limit: int = field(
        default_factory=lambda: _int("TOOL_BUFFER_LIMIT", 100000)
    )
    tool_tag_open: str = "<invoke"
    tool_tag_close: str = "</invoke>"
    tool_param_open: str = "<parameter"
    tool_param_close: str = "</parameter>"
    title_prompt_marker: str = "You name chat sessions"
    thread_prefix: str = "thread_"
    title_prefix: str = "title_"
    thread_hash_len: int = 24
    models: List[Dict[str, Any]] = field(
        default_factory=lambda: list(_TOML.get("MODELS", []))
    )
    state_path: Path = field(
        default_factory=lambda: Path(
            _env("STATE_PATH", str(ROOT / "session_state.json"))
        )
    )
    prompts: Dict[str, Any] = field(
        default_factory=lambda: dict(_TOML.get("PROMPTS", {}))
    )
    log_level: str = field(default_factory=lambda: _env("LOG_LEVEL", "INFO").upper())
    log_format: str = field(
        default_factory=lambda: _env("LOG_FORMAT", "pretty").lower()
    )
    debug: bool = field(
        default_factory=lambda: _to_bool(_env("DEBUG", False))
    )

    @property
    def tool_call_template(self) -> str:
        return (
            f'{self.tool_tag_open} name="tool_name">\n'
            f'{self.tool_param_open} name="param1">value1{self.tool_param_close}\n'
            f'{self.tool_param_open} name="param2">value2{self.tool_param_close}\n'
            f"{self.tool_tag_close}"
        )


settings = Settings()

DEEPSEEK_TOKEN = settings.deepseek_token
DEEPSEEK_EMAIL = settings.deepseek_email
DEEPSEEK_PASSWORD = settings.deepseek_password
MODEL_TYPE = settings.model_type
PROXY_HOST = settings.proxy_host
PROXY_PORT = settings.proxy_port
REQUEST_DELAY = settings.request_delay
TOOL_REMINDER_INTERVAL = settings.tool_reminder_interval
TOOL_BUFFER_LIMIT = settings.tool_buffer_limit
TOOL_TAG_OPEN = settings.tool_tag_open
TOOL_TAG_CLOSE = settings.tool_tag_close
TOOL_PARAM_OPEN = settings.tool_param_open
TOOL_PARAM_CLOSE = settings.tool_param_close
TOOL_CALL_TEMPLATE = settings.tool_call_template
TOOL_CALL_PREFIX = TOOL_TAG_OPEN
TOOL_CALL_SUFFIX = TOOL_TAG_CLOSE
MODELS = settings.models
STATE_PATH = settings.state_path

_enc = tiktoken.get_encoding("cl100k_base")


def estimate_tokens(text: str) -> int:
    if not text:
        return 0
    return len(_enc.encode(text, disallowed_special=()))


def render_prompt(template: str, **ctx: Any) -> str:
    """Substitute {placeholders} in a prompt template, including the real
    tool-call tag strings (resolved from Settings). Tag tokens stored as
    TOOL_CALL_OPEN/CLOSE/TOOL_PARAM_OPEN/CLOSE in the config file are replaced
    with their actual values."""
    resolved = dict(ctx)
    resolved.setdefault("tool_call_template", settings.tool_call_template)
    resolved.setdefault("tools_block", "")
    out = template
    # Replace placeholder token names in the config prose with the real tags.
    tag_map = {
        "TOOL_CALL_CLOSE": settings.tool_tag_close,
        "TOOL_CALL_OPEN": settings.tool_tag_open,
        "TOOL_PARAM_CLOSE": settings.tool_param_close,
        "TOOL_PARAM_OPEN": settings.tool_param_open,
    }
    for token, value in tag_map.items():
        out = out.replace(token, value)
    return out.format(**resolved)
