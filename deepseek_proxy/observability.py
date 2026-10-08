"""Shared logging setup for the DeepSeek proxy.

Two modes, selected by config:
  - log_format = "json"    -> structured JSON lines (prod default; greppable,
                              machine-parseable, request-id + timing fields).
  - log_format = "pretty"  -> colored, human-readable lines (dev/debug).

Level comes from LOG_LEVEL. Formatters include correlation and timing fields,
never arbitrary extras or message argument copies. Tracebacks require DEBUG.

Usage:  from logger import get_logger; log = get_logger(__name__)
"""

from __future__ import annotations

import json
import logging
import sys
import time
from contextlib import contextmanager
from contextvars import ContextVar, Token
from datetime import datetime, timezone
from typing import Any, Dict, Optional

from .settings import settings

# Context-local request id, so concurrent requests don't interleave.
_request_id: ContextVar[Optional[str]] = ContextVar("request_id", default=None)

# Structured fields promoted out of the log message into their own JSON keys
# (or appended as compact `k=v` in pretty mode). A fixed allowlist: the
# formatters never copy arbitrary `extra` attributes off a record, so a caller
# cannot leak payloads into the log by accident.
#
# NOTE: these must not collide with LogRecord's own attributes (`thread`,
# `name`, `module`, `process`, ...), which logging rejects in `extra`.
STRUCTURED_FIELDS = (
    "thread_id",
    "model",
    "stream",
    "prompt_tokens",
    "reanchor",
    "new_messages",
    "attempt",
    "unresolved",
    "remaining",
)


def _structured(record: logging.LogRecord) -> Dict[str, Any]:
    return {
        f: getattr(record, f) for f in STRUCTURED_FIELDS if getattr(record, f, None) is not None
    }


# ANSI colors for the pretty formatter.
_RESET = "\x1b[0m"
_COLORS = {
    "DEBUG": "\x1b[36m",  # cyan
    "INFO": "\x1b[32m",  # green
    "WARNING": "\x1b[33m",  # yellow
    "ERROR": "\x1b[31m",  # red
    "CRITICAL": "\x1b[35m",  # magenta
}


class JsonFormatter(logging.Formatter):
    """Structured JSON lines with request id and elapsed-time fields."""

    def __init__(self) -> None:
        super().__init__()

    def format(self, record: logging.LogRecord) -> str:
        payload: Dict[str, Any] = {
            "time": datetime.fromtimestamp(record.created, timezone.utc).isoformat(
                timespec="milliseconds"
            ),
            "level": record.levelname,
            "logger": record.name,
            "msg": record.getMessage(),
        }
        payload.update(_structured(record))
        rid = _request_id.get()
        if rid:
            payload["request_id"] = rid
        for attr in ("duration_ms", "elapsed_ms"):
            v = getattr(record, attr, None)
            if v is not None:
                payload[attr] = v
        if record.exc_info and settings.debug:
            payload["exc_info"] = self.formatException(record.exc_info)
        return json.dumps(payload, ensure_ascii=False)


class PrettyFormatter(logging.Formatter):
    """Colored, human-readable lines."""

    def format(self, record: logging.LogRecord) -> str:
        color = _COLORS.get(record.levelname, _RESET)
        base = (
            f"{time.strftime('%H:%M:%S', time.localtime(record.created))}.{int(record.msecs):03d} "
            f"{color}{record.levelname:<8}{_RESET} {record.getMessage()}"
        )
        fields = _structured(record)
        if fields:
            base += " " + " ".join(f"{k}={v}" for k, v in fields.items())
        rid = _request_id.get()
        if rid:
            base = f"[{rid}] {base}"
        for attr in ("duration_ms", "elapsed_ms"):
            value = getattr(record, attr, None)
            if value is not None:
                base += f" {attr}={value}"
        if record.exc_info and settings.debug:
            base += "\n" + self.formatException(record.exc_info)
        return base


def _configure() -> logging.Logger:
    root = logging.getLogger()
    root.setLevel(getattr(logging, settings.log_level.upper(), logging.INFO))
    root.handlers.clear()

    handler = logging.StreamHandler(sys.stdout)
    if settings.log_format == "json":
        handler.setFormatter(JsonFormatter())
    else:
        handler.setFormatter(PrettyFormatter())
    root.addHandler(handler)

    # Quiet the noisy default loggers unless debugging.
    if not settings.debug:
        for noisy in ("uvicorn.access", "aiodeepseek", "aiohttp", "httpx"):
            logging.getLogger(noisy).setLevel(logging.WARNING)

    return root


_root_configured = False
_configured_logger: Optional[logging.Logger] = None


def get_logger(name: str) -> logging.Logger:
    global _root_configured, _configured_logger
    if not _root_configured:
        _configured_logger = _configure()
        _root_configured = True
    return logging.getLogger(name)


def set_request_id(rid: str) -> Token:
    """Set an opaque correlation ID and return the token for nested restoration."""
    return _request_id.set(rid)


def clear_request_id(token: Optional[Token] = None) -> None:
    if token is None:
        _request_id.set(None)
    else:
        _request_id.reset(token)


def reset_request_id(token: Token) -> None:
    clear_request_id(token)


@contextmanager
def request_context(rid: str):
    token = set_request_id(rid)
    try:
        yield
    finally:
        clear_request_id(token)


def timed(log: logging.Logger, level: int, msg: str, *args: Any) -> None:
    """Log a message carrying elapsed-ms since process start (rough timing)."""
    extra = {"elapsed_ms": round((time.monotonic() - _START) * 1000, 3)}
    log.log(level, msg, *args, extra=extra)


_START = time.monotonic()
