"""Connection pool: N DeepSeek accounts behind one OpenAI-compatible facade.

Each account is a `Connection` with its own `aiodeepseek.DeepSeekClient`,
per-connection rate gate, conversation map, and session map. The pool owns:

  - routing: thread_id -> account email (persisted in the StateStore)
  - selection: least-crowded account for a NEW thread (idle-TTL occupancy)
  - resume state and token/exchange counters (persisted per-thread)

This module mirrors `deepseek_client.py`'s public functions so `main.py`
swaps the singleton for the pool with a minimal diff. N=1 behaves identically
to the old singleton.
"""

from __future__ import annotations

import asyncio
import threading
import time
from dataclasses import dataclass, field
from typing import AsyncGenerator, Dict, Optional

from aiodeepseek import DeepSeekClient
from aiodeepseek.conversation import Conversation
from aiodeepseek.types.enums import ModelType
from aiodeepseek.types.exceptions import DeepSeekError

from config import ACCOUNTS, MODEL_TYPE, REQUEST_DELAY
from db import StateStore
from logger import get_logger

log = get_logger(__name__)

_model_map = {
    "DEFAULT": ModelType.DEFAULT,
    "EXPERT": ModelType.EXPERT,
    "VISION": ModelType.VISION,
}


def _resolve_model(model_id: str) -> Optional[ModelType]:
    key = (model_id or "").upper().strip()
    if not key:
        return None
    return _model_map.get(key)


@dataclass
class Connection:
    """One DeepSeek account: client + per-connection rate gate + per-account maps."""
    email: str
    password: str
    client: Optional[DeepSeekClient] = None
    conversations: Dict[str, Conversation] = field(default_factory=dict)
    thread_sessions: Dict[str, str] = field(default_factory=dict)
    rate_lock: asyncio.Lock = field(default_factory=asyncio.Lock)
    last_fire: float = 0.0
    map_lock: threading.Lock = field(default_factory=threading.Lock)

    async def ensure_client(self) -> DeepSeekClient:
        if self.client is None:
            self.client = DeepSeekClient(email=self.email, password=self.password, model=_resolve_model(MODEL_TYPE) or ModelType.DEFAULT)
            await self.client.__aenter__()
        return self.client

    async def acquire_rate_slot(self) -> None:
        if REQUEST_DELAY <= 0:
            return
        async with self.rate_lock:
            now = time.monotonic()
            wait = self.last_fire + REQUEST_DELAY - now
            if wait > 0:
                log.info("Rate gate [%s]: waiting %.2fs", self.email, wait)
                await asyncio.sleep(wait)
            self.last_fire = time.monotonic()


class ConnectionPool:
    def __init__(self, accounts: list[dict], store: StateStore, idle_timeout: float = 300.0):
        if not accounts:
            raise ValueError("No DeepSeek accounts configured")
        self._conns: list[Connection] = [Connection(email=a["email"], password=a["password"]) for a in accounts]
        self._store = store
        self._idle_timeout = idle_timeout
        self._thread_owner: Dict[str, Connection] = {}  # hot route cache

    # ---- routing ----
    def _find_conn(self, email: str) -> Connection:
        for c in self._conns:
            if c.email == email:
                return c
        raise ValueError(f"Unknown backend email: {email}")

    def _assign(self, thread_id: str) -> Connection:
        """Return the connection for a thread, assigning a NEW thread to the
        least-crowded account (fewest active threads within idle TTL)."""
        existing = self._thread_owner.get(thread_id)
        if existing is not None:
            return existing

        # Persisted binding survives restart.
        bound_email = self._store.get_binding(thread_id)
        if bound_email is not None:
            conn = self._find_conn(bound_email)
            self._thread_owner[thread_id] = conn
            return conn

        # New thread: least crowded.
        occupancy = self._store.occupancy(self._idle_timeout)
        # Ties broken by lowest index (deterministic spread).
        chosen = min(self._conns, key=lambda c: (occupancy.get(c.email, 0), self._conns.index(c)))
        self._store.bind(thread_id, chosen.email)
        self._thread_owner[thread_id] = chosen
        return chosen

    def _assign_ephemeral(self) -> Connection:
        """Return a throwaway connection for a title-gen request.

        Title requests are one-shot: they must NOT bind a thread or touch
        occupancy, because they would otherwise consume an account slot for a
        request that never returns. Pick the least-crowded account at this
        instant (ties -> lowest index) and hand it back without persisting.
        """
        occupancy = self._store.occupancy(self._idle_timeout)
        return min(self._conns, key=lambda c: (occupancy.get(c.email, 0), self._conns.index(c)))

    def route(self, thread_id: str) -> Connection:
        # Title-gen threads use the `title_` prefix and must not be bound.
        if thread_id.startswith("title_"):
            return self._assign_ephemeral()
        conn = self._assign(thread_id)
        self._store.touch(thread_id)
        return conn

    async def ensure_clients(self) -> None:
        for c in self._conns:
            await c.ensure_client()

    async def close(self) -> None:
        for c in self._conns:
            if c.client is not None:
                await c.client.__aexit__(None, None, None)
                c.client = None

    # ---- conversation + generation (per-account) ----
    async def get_or_create_conversation(self, conn: Connection, thread_id: str) -> Conversation:
        with conn.map_lock:
            conv = conn.conversations.get(thread_id)
            if conv is None:
                conv = conn.client.new_conversation()
                conn.conversations[thread_id] = conv
                saved = self._store.get_resume(thread_id)
                if saved:
                    sid, pid = saved
                    if sid:
                        conn.thread_sessions[thread_id] = sid
                    if pid:
                        conv._parent_message_id = pid
            return conv

    async def generate_response(self, thread_id: str, prompt: str, model: str = "", stream: bool = False):
        conn = self.route(thread_id)
        await conn.ensure_client()
        conv = await self.get_or_create_conversation(conn, thread_id)
        model_type = _resolve_model(model)

        session_id = conn.thread_sessions.get(thread_id)
        if session_id is None:
            session_id = await conn.client.create_chat_session(conn.client._token)
            with conn.map_lock:
                conn.thread_sessions[thread_id] = session_id

        saved_session = conn.client._session_id
        conn.client._session_id = session_id

        await conn.acquire_rate_slot()

        try:
            if stream:
                async for chunk in conv.ask_stream(prompt, model=model_type):
                    yield chunk
            else:
                response = await conv.ask(prompt, model=model_type)
                yield response.text
        except DeepSeekError as e:
            log.error("DeepSeek API error [%s] (%s thread=%s): %s",
                      type(e).__name__, conn.email, thread_id, repr(e))
            if "input_exceeds_limit" in str(e).lower():
                session_id = await conn.client.create_chat_session(conn.client._token)
                with conn.map_lock:
                    conn.thread_sessions[thread_id] = session_id
                    conv = conn.client.new_conversation()
                    conn.conversations[thread_id] = conv
                conn.client._session_id = session_id
                if stream:
                    async for chunk in conv.ask_stream(prompt, model=model_type):
                        yield chunk
                else:
                    response = await conv.ask(prompt, model=model_type)
                    yield response.text
                if conv.parent_message_id:
                    self._save_resume(conn, thread_id, session_id, conv.parent_message_id)
                return
            raise
        finally:
            if saved_session is not None:
                conn.client._session_id = saved_session

        if conv.parent_message_id:
            self._save_resume(conn, thread_id, session_id, conv.parent_message_id)

    def _save_resume(self, conn: Connection, thread_id: str, session_id: str, parent_message_id: str) -> None:
        # Title-gen threads are ephemeral — never persist their resume state.
        if thread_id.startswith("title_"):
            return
        self._store.set_resume(thread_id, conn.email, session_id, parent_message_id)

    # ---- token / exchange counters (delegated to store) ----
    def get_thread_tokens(self, thread_id: str) -> int:
        return self._store.get_thread_tokens(thread_id)

    def add_thread_tokens(self, thread_id: str, n: int) -> None:
        self._store.add_thread_tokens(thread_id, n)

    def get_thread_exchanges(self, thread_id: str) -> int:
        return self._store.get_thread_exchanges(thread_id)

    def bump_thread_exchanges(self, thread_id: str, n: int) -> None:
        self._store.bump_thread_exchanges(thread_id, n)


# ---- module-level facade (mirrors deepseek_client.py) ----
_pool: Optional[ConnectionPool] = None


def init_pool(db_path: str, idle_timeout: float = 300.0, snapshot_interval: float = 5.0) -> ConnectionPool:
    global _pool
    store = StateStore(db_path, snapshot_interval=snapshot_interval)
    store.open()
    _pool = ConnectionPool(ACCOUNTS, store, idle_timeout=idle_timeout)
    return _pool


def get_pool() -> ConnectionPool:
    if _pool is None:
        raise RuntimeError("Pool not initialized — call init_pool() at startup")
    return _pool


async def shutdown_pool() -> None:
    global _pool
    if _pool is not None:
        await _pool.close()
        _pool._store.close()
        _pool = None


async def generate_response(thread_id: str, prompt: str, model: str = "", stream: bool = False):
    pool = get_pool()
    async for chunk in pool.generate_response(thread_id, prompt, model=model, stream=stream):
        yield chunk


def get_thread_tokens(thread_id: str) -> int:
    return get_pool().get_thread_tokens(thread_id)


def add_thread_tokens(thread_id: str, n: int) -> None:
    get_pool().add_thread_tokens(thread_id, n)


def get_thread_exchanges(thread_id: str) -> int:
    return get_pool().get_thread_exchanges(thread_id)


def bump_thread_exchanges(thread_id: str, n: int) -> None:
    get_pool().bump_thread_exchanges(thread_id, n)
