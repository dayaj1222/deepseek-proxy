"""SQLite state store for thread→account binding and per-account resume state.

Single source of truth on disk, with a hot in-memory cache for reads. Runtime
updates hit memory first and are snapshotted to SQLite on a debounce (and on
shutdown), so the per-request disk cost is zero.

Two tables:

  threads(thread_id, backend_email, last_active, created_at)
      Routing truth: which account (email) owns a thread. last_active is the
      epoch of the most recent request, used for idle-TTL occupancy.

  resume(thread_id, backend_email, session_id, parent_message_id)
      DeepSeek continuation tokens, keyed by thread. Written by whichever
      connection owns the thread so it can resume after restart.

Threads never leave `threads` (the binding is permanent); idle threads are
simply not counted by the occupancy query.
"""

from __future__ import annotations

import sqlite3
import threading
import time
from pathlib import Path
from typing import Dict, Optional, Tuple


_SCHEMA = """
CREATE TABLE IF NOT EXISTS threads (
    thread_id    TEXT PRIMARY KEY,
    backend_email TEXT NOT NULL,
    last_active  REAL NOT NULL,
    created_at   REAL NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_threads_backend_active
    ON threads(backend_email, last_active);

CREATE TABLE IF NOT EXISTS resume (
    thread_id         TEXT PRIMARY KEY,
    backend_email     TEXT NOT NULL,
    session_id        TEXT,
    parent_message_id TEXT,
    total_tokens      INTEGER NOT NULL DEFAULT 0,
    exchanges         INTEGER NOT NULL DEFAULT 0
);
"""


class StateStore:
    """In-memory + debounced-SQLite store. Thread-safe within one process."""

    def __init__(self, db_path: str, snapshot_interval: float = 5.0):
        self._db_path = Path(db_path)
        self._snapshot_interval = snapshot_interval
        self._lock = threading.RLock()
        self._conn: Optional[sqlite3.Connection] = None

        # Hot cache (source of truth during operation).
        self._threads: Dict[str, Dict] = {}   # thread_id -> {backend_email, last_active, created_at}
        self._resume: Dict[str, Dict] = {}    # thread_id -> {session_id, parent_message_id}
        self._dirty = False
        self._last_snapshot = 0.0

    # ---- lifecycle ----
    def open(self) -> None:
        self._db_path.parent.mkdir(parents=True, exist_ok=True)
        self._conn = sqlite3.connect(str(self._db_path), check_same_thread=False)
        self._conn.execute("PRAGMA journal_mode=WAL;")
        self._conn.execute("PRAGMA busy_timeout=5000;")
        self._conn.executescript(_SCHEMA)
        self._load()

    def close(self) -> None:
        self.snapshot()
        if self._conn is not None:
            self._conn.close()
            self._conn = None

    def _load(self) -> None:
        cur = self._conn.execute("SELECT thread_id, backend_email, last_active, created_at FROM threads")
        for tid, email, last_active, created_at in cur.fetchall():
            self._threads[tid] = {
                "backend_email": email,
                "last_active": last_active,
                "created_at": created_at,
            }
        cur = self._conn.execute("SELECT thread_id, session_id, parent_message_id, total_tokens, exchanges FROM resume")
        for tid, sid, pid, tokens, exchanges in cur.fetchall():
            self._resume[tid] = {
                "session_id": sid,
                "parent_message_id": pid,
                "total_tokens": tokens,
                "exchanges": exchanges,
            }

    # ---- binding (thread -> account) ----
    def get_binding(self, thread_id: str) -> Optional[str]:
        with self._lock:
            entry = self._threads.get(thread_id)
            return entry["backend_email"] if entry else None

    def bind(self, thread_id: str, backend_email: str) -> None:
        with self._lock:
            now = time.time()
            self._threads[thread_id] = {
                "backend_email": backend_email,
                "last_active": now,
                "created_at": now,
            }
            self._mark_dirty(now)

    def touch(self, thread_id: str) -> None:
        """Update last_active on a request (debounced snapshot)."""
        with self._lock:
            entry = self._threads.get(thread_id)
            if entry is None:
                return
            entry["last_active"] = time.time()

    def occupancy(self, idle_timeout: float) -> Dict[str, int]:
        """Count active threads per backend, idle-TTL applied."""
        now = time.time()
        cutoff = now - idle_timeout
        counts: Dict[str, int] = {}
        with self._lock:
            for entry in self._threads.values():
                if entry["last_active"] >= cutoff:
                    email = entry["backend_email"]
                    counts[email] = counts.get(email, 0) + 1
        return counts

    # ---- resume state ----
    def get_resume(self, thread_id: str) -> Optional[Tuple[Optional[str], Optional[str]]]:
        with self._lock:
            entry = self._resume.get(thread_id)
            if entry is None:
                return None
            return (entry.get("session_id"), entry.get("parent_message_id"))

    def set_resume(self, thread_id: str, backend_email: str,
                   session_id: Optional[str], parent_message_id: Optional[str]) -> None:
        with self._lock:
            now = time.time()
            entry = self._resume.get(thread_id, {})
            entry.update({
                "backend_email": backend_email,
                "session_id": session_id,
                "parent_message_id": parent_message_id,
            })
            entry.setdefault("total_tokens", 0)
            entry.setdefault("exchanges", 0)
            self._resume[thread_id] = entry
            self._mark_dirty(now)

    # ---- token / exchange counters ----
    def get_thread_tokens(self, thread_id: str) -> int:
        with self._lock:
            return int(self._resume.get(thread_id, {}).get("total_tokens", 0))

    def add_thread_tokens(self, thread_id: str, n: int) -> None:
        if not n:
            return
        with self._lock:
            entry = self._resume.setdefault(thread_id, {"total_tokens": 0, "exchanges": 0})
            entry["total_tokens"] = int(entry.get("total_tokens", 0)) + n
            self._mark_dirty(time.time())

    def get_thread_exchanges(self, thread_id: str) -> int:
        with self._lock:
            return int(self._resume.get(thread_id, {}).get("exchanges", 0))

    def bump_thread_exchanges(self, thread_id: str, n: int) -> None:
        if not n:
            return
        with self._lock:
            entry = self._resume.setdefault(thread_id, {"total_tokens": 0, "exchanges": 0})
            entry["exchanges"] = int(entry.get("exchanges", 0)) + n
            self._mark_dirty(time.time())

    # ---- persistence ----
    def _mark_dirty(self, now: float) -> None:
        self._dirty = True
        if now - self._last_snapshot >= self._snapshot_interval:
            self.snapshot()

    def snapshot(self) -> None:
        with self._lock:
            if self._conn is None or not self._dirty:
                return
            self._conn.execute("BEGIN")
            try:
                for tid, e in self._threads.items():
                    self._conn.execute(
                        "INSERT OR REPLACE INTO threads (thread_id, backend_email, last_active, created_at) "
                        "VALUES (?, ?, ?, ?)",
                        (tid, e["backend_email"], e["last_active"], e["created_at"]),
                    )
                for tid, e in self._resume.items():
                    self._conn.execute(
                        "INSERT OR REPLACE INTO resume (thread_id, backend_email, session_id, parent_message_id, total_tokens, exchanges) "
                        "VALUES (?, ?, ?, ?, ?, ?)",
                        (
                            tid,
                            e.get("backend_email", ""),
                            e.get("session_id"),
                            e.get("parent_message_id"),
                            int(e.get("total_tokens", 0)),
                            int(e.get("exchanges", 0)),
                        ),
                    )
                self._conn.execute("COMMIT")
            except Exception:
                self._conn.execute("ROLLBACK")
                raise
            self._dirty = False
            self._last_snapshot = time.time()
