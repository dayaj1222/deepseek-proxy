"""State store for thread→account binding and per-account resume state.

Single logical source of truth, with a hot in-memory cache for reads.
Runtime updates hit memory only. The application periodically calls
snapshot() (and closes on shutdown); only changed rows are persisted.

Two logical tables (regardless of backend):

  threads(thread_id, backend_email, last_active, created_at)
      Routing truth: which account (email) owns a thread. last_active is the
      epoch of the most recent request, used for idle-TTL occupancy.

  resume(thread_id, backend_email, session_id, parent_message_id,
         total_tokens, exchanges)
      DeepSeek continuation tokens, keyed by thread. Written by whichever
      connection owns the thread so it can resume after restart.

Threads never leave `threads` (the binding is permanent); idle threads are
simply not counted by the occupancy query.

Persistence is pluggable behind StorageBackend:

  - SqliteBackend  (default; local runs, tests)
  - MongoBackend   (deployment, e.g. MongoDB Atlas free tier)

Select with STORAGE_BACKEND=sqlite|mongo. See settings.py.
"""

from __future__ import annotations

import sqlite3
import threading
import time
from pathlib import Path
from typing import Dict, Optional, Protocol, Tuple


# ---------------------------------------------------------------------------
# Backend protocol
# ---------------------------------------------------------------------------

ThreadsMap = Dict[str, Dict]
ResumeMap = Dict[str, Dict]


class StorageBackend(Protocol):
    """Persistence seam. StateStore owns the caches; the backend owns disk/network.

    Contract:
      - open() runs once before any load()/write() call.
      - load() returns (threads, resume) as plain dicts keyed by thread_id.
      - write(threads, resume) persists only the supplied (already-diffed) rows.
      - close() flushes and releases resources; safe to call once.
    """

    def open(self) -> None: ...

    def load(self) -> Tuple[ThreadsMap, ResumeMap]: ...

    def write(self, threads: ThreadsMap, resume: ResumeMap) -> None: ...

    def close(self) -> None: ...

    def insert_api_key(self, record: Dict) -> None: ...

    def list_api_keys(self) -> list: ...

    def find_api_key_by_hash(self, key_hash: str) -> Optional[Dict]: ...

    def delete_api_key(self, key_id: str) -> bool: ...

    def touch_api_key_used(self, key_id: str, when: float) -> None: ...


# ---------------------------------------------------------------------------
# SQLite backend (previous StateStore persistence, unchanged in behavior)
# ---------------------------------------------------------------------------

_SQLITE_SCHEMA = """
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

CREATE TABLE IF NOT EXISTS api_keys (
    id            TEXT PRIMARY KEY,
    key_hash      TEXT NOT NULL UNIQUE,
    key_prefix    TEXT NOT NULL,
    name          TEXT NOT NULL,
    created_at    REAL NOT NULL,
    last_used_at  REAL
);
"""


class SqliteBackend:
    """SQLite persistence. One connection, WAL mode, INSERT OR REPLACE upserts."""

    def __init__(self, db_path: str):
        self._db_path = Path(db_path)
        self._conn: Optional[sqlite3.Connection] = None
        self._lock = threading.RLock()

    @property
    def path(self) -> Path:
        return self._db_path

    def open(self) -> None:
        with self._lock:
            if self._conn is not None:
                return
            self._db_path.parent.mkdir(parents=True, exist_ok=True)
            self._conn = sqlite3.connect(str(self._db_path), check_same_thread=False)
            try:
                self._conn.execute("PRAGMA journal_mode=WAL;")
                self._conn.execute("PRAGMA busy_timeout=5000;")
                self._conn.executescript(_SQLITE_SCHEMA)
            except Exception:
                self._conn.close()
                self._conn = None
                raise

    def load(self) -> Tuple[ThreadsMap, ResumeMap]:
        with self._lock:
            if self._conn is None:
                raise RuntimeError("backend is not open")
            threads: ThreadsMap = {}
            resume: ResumeMap = {}
            cur = self._conn.execute(
                "SELECT thread_id, backend_email, last_active, created_at FROM threads"
            )
            for tid, email, last_active, created_at in cur.fetchall():
                threads[tid] = {
                    "backend_email": email,
                    "last_active": last_active,
                    "created_at": created_at,
                }
            cur = self._conn.execute(
                "SELECT thread_id, backend_email, session_id, parent_message_id, total_tokens, exchanges FROM resume"
            )
            for tid, email, sid, pid, tokens, exchanges in cur.fetchall():
                resume[tid] = {
                    "backend_email": email,
                    "session_id": sid,
                    "parent_message_id": pid,
                    "total_tokens": tokens,
                    "exchanges": exchanges,
                }
            return threads, resume

    def write(self, threads: ThreadsMap, resume: ResumeMap) -> None:
        with self._lock:
            if self._conn is None:
                raise RuntimeError("backend is not open")
            self._conn.execute("BEGIN")
            try:
                for tid, e in threads.items():
                    self._conn.execute(
                        "INSERT OR REPLACE INTO threads (thread_id, backend_email, last_active, created_at) "
                        "VALUES (?, ?, ?, ?)",
                        (tid, e["backend_email"], e["last_active"], e["created_at"]),
                    )
                for tid, e in resume.items():
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
                self._conn.rollback()
                raise

    def close(self) -> None:
        with self._lock:
            if self._conn is not None:
                self._conn.close()
                self._conn = None

    # ---- api keys ----
    def insert_api_key(self, record: Dict) -> None:
        with self._lock:
            if self._conn is None:
                raise RuntimeError("backend is not open")
            self._conn.execute(
                "INSERT INTO api_keys (id, key_hash, key_prefix, name, created_at, last_used_at) "
                "VALUES (?, ?, ?, ?, ?, ?)",
                (
                    record["id"],
                    record["key_hash"],
                    record["key_prefix"],
                    record["name"],
                    record["created_at"],
                    record.get("last_used_at"),
                ),
            )
            self._conn.commit()

    def list_api_keys(self) -> list:
        with self._lock:
            if self._conn is None:
                raise RuntimeError("backend is not open")
            cur = self._conn.execute(
                "SELECT id, key_prefix, name, created_at, last_used_at FROM api_keys "
                "ORDER BY created_at DESC"
            )
            cols = ("id", "key_prefix", "name", "created_at", "last_used_at")
            return [dict(zip(cols, row)) for row in cur.fetchall()]

    def find_api_key_by_hash(self, key_hash: str) -> Optional[Dict]:
        with self._lock:
            if self._conn is None:
                raise RuntimeError("backend is not open")
            cur = self._conn.execute(
                "SELECT id, key_hash, key_prefix, name, created_at, last_used_at FROM api_keys "
                "WHERE key_hash = ?",
                (key_hash,),
            )
            row = cur.fetchone()
            if row is None:
                return None
            cols = ("id", "key_hash", "key_prefix", "name", "created_at", "last_used_at")
            return dict(zip(cols, row))

    def delete_api_key(self, key_id: str) -> bool:
        with self._lock:
            if self._conn is None:
                raise RuntimeError("backend is not open")
            cur = self._conn.execute("DELETE FROM api_keys WHERE id = ?", (key_id,))
            self._conn.commit()
            return cur.rowcount > 0

    def touch_api_key_used(self, key_id: str, when: float) -> None:
        with self._lock:
            if self._conn is None:
                raise RuntimeError("backend is not open")
            self._conn.execute(
                "UPDATE api_keys SET last_used_at = ? WHERE id = ?", (when, key_id)
            )
            self._conn.commit()


# ---------------------------------------------------------------------------
# MongoDB backend (deployment; e.g. MongoDB Atlas free tier)
# ---------------------------------------------------------------------------


class MongoBackend:
    """MongoDB persistence. Two collections, `_id = thread_id`.

    Uses the sync pymongo driver, matching the sync StateStore API; call sites
    that need to keep the event loop free wrap snapshot() in asyncio.to_thread,
    exactly as they do for SQLite. Writes are bulk upserts of only the changed
    rows, so this preserves the debounced-flush semantics.

    pymongo is an optional dependency: installed only when
    STORAGE_BACKEND=mongo is actually used.
    """

    def __init__(self, uri: str, database: str = "deepseek_proxy"):
        self._uri = uri
        self._database = database
        self._client = None
        self._threads = None
        self._resume = None
        self._api_keys = None

    def open(self) -> None:
        try:
            from pymongo import MongoClient
        except ImportError as exc:  # pragma: no cover - environment dependent
            raise RuntimeError(
                "STORAGE_BACKEND=mongo requires pymongo; install it with "
                "`pip install pymongo` (or the project's mongo extra)"
            ) from exc
        if not self._uri:
            raise RuntimeError("MONGODB_URI must be set when STORAGE_BACKEND=mongo")
        self._client = MongoClient(self._uri, appname="deepseek-proxy", tz_aware=False)
        db = self._client[self._database]
        self._threads = db["threads"]
        self._resume = db["resume"]
        self._api_keys = db["api_keys"]
        self._api_keys.create_index("key_hash", unique=True)
        # Fail fast on bad credentials / unreachable cluster rather than on the
        # first flush, which is fire-and-forget from the app's perspective.
        self._client.admin.command("ping")

    def load(self) -> Tuple[ThreadsMap, ResumeMap]:
        if self._threads is None or self._resume is None:
            raise RuntimeError("backend is not open")
        threads: ThreadsMap = {}
        resume: ResumeMap = {}
        for doc in self._threads.find({}):
            threads[doc["_id"]] = {
                "backend_email": doc.get("backend_email", ""),
                "last_active": float(doc.get("last_active", 0.0)),
                "created_at": float(doc.get("created_at", 0.0)),
            }
        for doc in self._resume.find({}):
            resume[doc["_id"]] = {
                "backend_email": doc.get("backend_email", ""),
                "session_id": doc.get("session_id"),
                "parent_message_id": doc.get("parent_message_id"),
                "total_tokens": int(doc.get("total_tokens", 0)),
                "exchanges": int(doc.get("exchanges", 0)),
            }
        return threads, resume

    def write(self, threads: ThreadsMap, resume: ResumeMap) -> None:
        if self._threads is None or self._resume is None:
            raise RuntimeError("backend is not open")
        from pymongo import UpdateOne

        thread_ops = []
        for tid, e in threads.items():
            thread_ops.append(
                UpdateOne(
                    {"_id": tid},
                    {
                        "$set": {
                            "backend_email": e["backend_email"],
                            "last_active": e["last_active"],
                            "created_at": e["created_at"],
                        }
                    },
                    upsert=True,
                )
            )
        resume_ops = []
        for tid, e in resume.items():
            resume_ops.append(
                UpdateOne(
                    {"_id": tid},
                    {
                        "$set": {
                            "backend_email": e.get("backend_email", ""),
                            "session_id": e.get("session_id"),
                            "parent_message_id": e.get("parent_message_id"),
                            "total_tokens": int(e.get("total_tokens", 0)),
                            "exchanges": int(e.get("exchanges", 0)),
                        }
                    },
                    upsert=True,
                )
            )
        if thread_ops:
            self._threads.bulk_write(thread_ops, ordered=False)
        if resume_ops:
            self._resume.bulk_write(resume_ops, ordered=False)

    # ---- api keys ----
    def insert_api_key(self, record: Dict) -> None:
        if self._api_keys is None:
            raise RuntimeError("backend is not open")
        self._api_keys.insert_one(dict(record))

    def list_api_keys(self) -> list:
        if self._api_keys is None:
            raise RuntimeError("backend is not open")
        cur = self._api_keys.find({}, {"key_hash": 0}).sort("created_at", -1)
        return [{k: v for k, v in doc.items() if k != "_id"} for doc in cur]

    def find_api_key_by_hash(self, key_hash: str) -> Optional[Dict]:
        if self._api_keys is None:
            raise RuntimeError("backend is not open")
        doc = self._api_keys.find_one({"key_hash": key_hash})
        if doc is None:
            return None
        return {k: v for k, v in doc.items() if k != "_id"}

    def delete_api_key(self, key_id: str) -> bool:
        if self._api_keys is None:
            raise RuntimeError("backend is not open")
        return self._api_keys.delete_one({"id": key_id}).deleted_count > 0

    def touch_api_key_used(self, key_id: str, when: float) -> None:
        if self._api_keys is None:
            raise RuntimeError("backend is not open")
        self._api_keys.update_one({"id": key_id}, {"$set": {"last_used_at": when}})

    def close(self) -> None:
        if self._client is not None:
            self._client.close()
            self._client = None
            self._threads = None
            self._resume = None
            self._api_keys = None


# ---------------------------------------------------------------------------
# Backend factory
# ---------------------------------------------------------------------------


def build_backend(kind: str, *, db_path: str, mongo_uri: str = "", mongo_db: str = "deepseek_proxy") -> StorageBackend:
    """Select a persistence backend by name. Defaults to SQLite for local runs."""
    normalized = (kind or "sqlite").strip().lower()
    if normalized == "sqlite":
        return SqliteBackend(db_path)
    if normalized == "mongo":
        return MongoBackend(mongo_uri, mongo_db)
    raise ValueError(f"unknown STORAGE_BACKEND {kind!r}; expected 'sqlite' or 'mongo'")


# ---------------------------------------------------------------------------
# StateStore — caches + public API; delegates persistence to a backend
# ---------------------------------------------------------------------------


class StateStore:
    """In-memory + debounced persistence. Thread-safe within one process.

    The public API is unchanged: all routing/resume reads and mutations are
    served from the hot cache, and only `snapshot()`/`close()` touch the
    backend.
    """

    def __init__(self, db_path: str, snapshot_interval: float = 5.0, backend: Optional[StorageBackend] = None):
        self._snapshot_interval = snapshot_interval
        self._lock = threading.RLock()
        self._flush_lock = threading.RLock()
        self._backend: StorageBackend = backend or SqliteBackend(db_path)

        # Hot cache (source of truth during operation).
        self._threads: ThreadsMap = {}  # thread_id -> {backend_email, last_active, created_at}
        self._resume: ResumeMap = {}  # thread_id -> {session_id, parent_message_id, ...}
        self._dirty = False
        self._dirty_threads: set[str] = set()
        self._dirty_resume: set[str] = set()
        self._last_snapshot = 0.0

    # ---- lifecycle ----
    @property
    def _conn(self) -> sqlite3.Connection:
        """Backward-compat accessor for tests and legacy callers.

        Only valid when the SQLite backend is in use. Keeps existing tests that
        inspect the connection (total_changes, triggers, trace callbacks) working
        unchanged after persistence was factored into SqliteBackend.
        """
        backend = self._backend
        conn = getattr(backend, "_conn", None)
        if conn is None:
            raise AttributeError(
                "_conn is only available with the SQLite backend"
            )
        return conn

    def open(self) -> None:
        with self._flush_lock, self._lock:
            self._backend.open()
            threads, resume = self._backend.load()
            self._threads.update(threads)
            self._resume.update(resume)

    def close(self) -> None:
        with self._flush_lock:
            self.snapshot()
            self._backend.close()

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
                "created_at": self._threads.get(thread_id, {}).get("created_at", now),
            }
            self._dirty_threads.add(thread_id)
            self._mark_dirty(now)

    def touch(self, thread_id: str) -> None:
        """Update last_active in memory, marking the binding for the next flush."""
        with self._lock:
            entry = self._threads.get(thread_id)
            if entry is None:
                return
            entry["last_active"] = time.time()
            self._dirty_threads.add(thread_id)
            self._mark_dirty(entry["last_active"])

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

    def set_resume(
        self,
        thread_id: str,
        backend_email: str,
        session_id: Optional[str],
        parent_message_id: Optional[str],
    ) -> None:
        with self._lock:
            now = time.time()
            entry = self._resume.get(thread_id, {})
            entry.update(
                {
                    "backend_email": backend_email,
                    "session_id": session_id,
                    "parent_message_id": parent_message_id,
                }
            )
            entry.setdefault("total_tokens", 0)
            entry.setdefault("exchanges", 0)
            self._resume[thread_id] = entry
            self._dirty_resume.add(thread_id)
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
            self._dirty_resume.add(thread_id)
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
            self._dirty_resume.add(thread_id)
            self._mark_dirty(time.time())

    # ---- persistence ----
    def _mark_dirty(self, now: float) -> None:
        self._dirty = True

    def snapshot(self) -> None:
        """Flush changed rows to the backend; safe to run via asyncio.to_thread.

        Serialize backend access separately from the cache lock so requests can
        continue updating memory during I/O. Failed rows remain dirty.
        """
        with self._flush_lock:
            with self._lock:
                if not self._dirty:
                    return
                threads = {tid: self._threads[tid].copy() for tid in self._dirty_threads}
                resume = {tid: self._resume[tid].copy() for tid in self._dirty_resume}
                for tid, entry in resume.items():
                    if not entry.get("backend_email"):
                        entry["backend_email"] = self._threads.get(tid, {}).get("backend_email", "")
                self._dirty_threads.clear()
                self._dirty_resume.clear()
                self._dirty = False
            try:
                self._backend.write(threads, resume)
            except Exception:
                with self._lock:
                    self._dirty_threads.update(threads)
                    self._dirty_resume.update(resume)
                    self._dirty = True
                raise
            self._last_snapshot = time.time()

    def flush(self) -> None:
        """Explicit persistence hook for the application's periodic lifecycle."""
        self.snapshot()
