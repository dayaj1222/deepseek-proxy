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
import os
import sys
import threading
import time
from collections import deque
from contextlib import contextmanager
from contextvars import ContextVar, Token
from datetime import datetime, timezone
from typing import Any, Deque, Dict, List, Optional

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


# ---------------------------------------------------------------------------
# In-memory log buffer for the admin UI (/admin/logs).
#
# The proxy logs to stdout only; in --bg mode the fish wrapper redirects that
# to .run/deepseek-proxy.log. The admin UI cannot rely on that file existing
# (and cannot tail it cheaply from async code), so a small ring buffer keeps
# the most recent records in process instead. It starts empty on restart and
# is deliberately bounded: ADMIN_LOG_BUFFER records, 0 disables it entirely.
# ---------------------------------------------------------------------------


def _buffer_size() -> int:
    # Deliberately small: this is a human-facing tail, not a log store. Older
    # records are discarded from memory once the ring is full.
    raw = _env_int("ADMIN_LOG_BUFFER", 50)
    return max(0, raw)


def _env_int(name: str, default: int) -> int:
    raw = os.environ.get(name)
    if raw is None or not raw.strip():
        return default
    try:
        return int(raw)
    except ValueError:
        return default


class LogRingHandler(logging.Handler):
    """Keep the most recent records in a bounded, thread-safe ring buffer.

    Records are stored as JSON-ready dicts shaped like the JSON formatter's
    output, so the admin UI can render them without reimplementing formatting.
    Every record carries a monotonically increasing ``seq`` so clients can
    poll incrementally with ``?after=<seq>`` instead of refetching.
    """

    def __init__(self, capacity: int = 1000) -> None:
        super().__init__()
        self.capacity = capacity
        self._records: Deque[Dict[str, Any]] = deque(maxlen=capacity or 1)
        self._lock = threading.Lock()
        self._seq = 0

    def emit(self, record: logging.LogRecord) -> None:
        try:
            entry = self._entry(record)
        except Exception:  # pragma: no cover - logging must never raise
            self.handleError(record)
            return
        if self.capacity <= 0:
            return
        with self._lock:
            self._seq += 1
            entry["seq"] = self._seq
            self._records.append(entry)

    def _entry(self, record: logging.LogRecord) -> Dict[str, Any]:
        entry: Dict[str, Any] = {
            "time": record.created,
            "level": record.levelname,
            "logger": record.name,
            "msg": record.getMessage(),
        }
        entry.update(_structured(record))
        # Prefer an explicit record attribute (lets callers label a record
        # directly); fall back to the request-scoped contextvar.
        rid = getattr(record, "request_id", None) or _request_id.get()
        if rid:
            entry["request_id"] = rid
        for attr in ("duration_ms", "elapsed_ms"):
            value = getattr(record, attr, None)
            if value is not None:
                entry[attr] = value
        if record.exc_info and settings.debug:
            entry["exc_info"] = self.formatException(record.exc_info)
        return entry

    @property
    def last_seq(self) -> int:
        with self._lock:
            return self._seq

    def snapshot(
        self,
        after: int = 0,
        level: Optional[str] = None,
        request_id: Optional[str] = None,
        limit: Optional[int] = None,
    ) -> List[Dict[str, Any]]:
        """Return buffered records newer than ``after``, oldest first.

        ``level`` is a minimum level name (e.g. "WARNING"); ``request_id``
        filters exactly. Filtering happens after the ``after`` cut so callers
        can page without missing records that were filtered out.
        """
        threshold = _level_value(level)
        with self._lock:
            records = [r for r in self._records if r["seq"] > after]
        if threshold is not None:
            records = [r for r in records if _level_value(r["level"]) >= threshold]
        if request_id:
            records = [r for r in records if r.get("request_id") == request_id]
        if limit is not None and limit > 0:
            records = records[-limit:]
        return records


def _level_value(name: Optional[str]) -> Optional[int]:
    if not name:
        return None
    # getLevelName returns an int for known names but a "Level X" *string*
    # for unknown ones, so it cannot be used as a validity check directly.
    value = logging.getLevelName(str(name).upper())
    return value if isinstance(value, int) else None


def get_log_buffer() -> Optional[LogRingHandler]:
    """Return the process-wide ring handler, or None when disabled."""
    return _log_buffer


_log_buffer: Optional[LogRingHandler] = None


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

    # Second sink: the bounded in-memory buffer backing /admin/logs. The ring
    # handler formats nothing itself (records stay structured), so it needs no
    # formatter. A zero capacity disables it.
    global _log_buffer
    capacity = _buffer_size()
    if capacity > 0:
        _log_buffer = LogRingHandler(capacity)
        root.addHandler(_log_buffer)
    else:
        _log_buffer = None

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
