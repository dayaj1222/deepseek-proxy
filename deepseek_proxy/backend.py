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
from contextlib import aclosing, asynccontextmanager
from dataclasses import dataclass, field
from typing import Dict, Optional

from aiodeepseek import DeepSeekClient
from aiodeepseek.conversation import Conversation
from aiodeepseek.types.enums import ModelType
from aiodeepseek.types.exceptions import DeepSeekError

from .settings import (
    ACCOUNTS,
    MODEL_TYPE,
    RATE_LIMIT_BACKOFF_S,
    RATE_LIMIT_MAX_RETRIES,
    REQUEST_DELAY,
)
from .storage import StateStore, build_backend
from .observability import get_logger

log = get_logger(__name__)


class BackendRateLimited(Exception):
    """DeepSeek backend throttled us. Mapped to HTTP 429 (retryable)."""

    def __init__(self, message: str = "Backend rate limit reached", retry_after: float = 30.0):
        super().__init__(message)
        self.retry_after = retry_after


class AccountQueueFull(BackendRateLimited):
    """The account's bounded waiting queue is full."""


class AccountQueueTimeout(BackendRateLimited):
    """The account did not become available before the queue deadline."""


_RATE_LIMIT_MARKERS = (
    "rate_limit",
    "rate limit",
    "too frequent",
    "too many",
    "429",
    "throttl",
    "overloaded",
    "busy",
    "generation_err",
    "server is temporarily unavailable",
)


def _is_rate_limit_error(e: Exception) -> bool:
    s = f"{type(e).__name__}: {e}".lower()
    return any(k in s for k in _RATE_LIMIT_MARKERS)


def _is_invalid_token_error(e: Exception) -> bool:
    s = f"{type(e).__name__}: {e}".lower()
    return "invalidtoken" in s or "invalid token" in s or "40003" in s


_LOGIN_MARKERS = (
    "login failed",
    "risk_device",
    "risk device",
    "biz_code=11",
    "biz_code': 11",
    "unauthorized",
    "auth failed",
    "authentication failed",
    "invalid password",
    "wrong password",
)


def _is_login_error(e: Exception) -> bool:
    s = f"{type(e).__name__}: {e}".lower()
    return any(k in s for k in _LOGIN_MARKERS)


# Login failures (bad password, banned/risky device) are not transient:
# park the account for an hour so new threads avoid it.
LOGIN_COOLDOWN_S = 3600.0


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
    generation_locks: Dict[str, asyncio.Lock] = field(default_factory=dict)
    request_delay: float = field(default_factory=lambda: max(0.0, REQUEST_DELAY))
    token: str | None = field(default=None, repr=False)
    queue_limit: int = 64
    queue_timeout: float = 120.0
    waiting: int = 0
    cooldown_until: float = 0.0

    async def ensure_client(self) -> DeepSeekClient:
        async with self.rate_slot():
            return await self._ensure_client()

    async def _ensure_client(self) -> DeepSeekClient:
        if self.client is None:
            credentials = (
                {"token": self.token}
                if self.token
                else {"email": self.email, "password": self.password}
            )
            client = DeepSeekClient(
                **credentials, model=_resolve_model(MODEL_TYPE) or ModelType.DEFAULT
            )
            try:
                await client.__aenter__()
            except BaseException:
                await client.__aexit__(None, None, None)
                raise
            self.client = client
        return self.client

    async def reconnect(self) -> DeepSeekClient:
        """Drop the client and log in fresh (recovers expired/invalid tokens)."""
        async with self.rate_slot():
            return await self._reconnect()

    async def _reconnect(self) -> DeepSeekClient:
        # All Conversation objects refer to the old client. Resume IDs remain
        # valid and are restored when each conversation is next requested.
        self.conversations.clear()
        if self.client is not None:
            client, self.client = self.client, None
            try:
                await client.__aexit__(None, None, None)
            except Exception as e:
                log.warning("Error closing client during reconnect: %s", e)
        return await self._ensure_client()

    async def acquire_rate_slot(self) -> None:
        """Queue this account's request and wait after the prior one ends.

        The lock is held around initialization, generation and cleanup.
        ``last_fire`` is set on release, so the configured delay measures
        completion-to-next-start rather than request-start-to-request-start.
        """
        if (self.rate_lock.locked() or self.waiting) and self.waiting >= self.queue_limit:
            raise AccountQueueFull("Account request queue is full")
        self.waiting += 1
        acquired = False
        try:
            async with asyncio.timeout(self.queue_timeout):
                await self.rate_lock.acquire()
                acquired = True
                self.waiting -= 1
                deadline = max(
                    self.last_fire + self.request_delay if self.last_fire else 0,
                    self.cooldown_until,
                )
                wait = deadline - time.monotonic()
                if wait > 0:
                    await asyncio.sleep(wait)
        except TimeoutError as e:
            if acquired:
                self.rate_lock.release()
            raise AccountQueueTimeout("Timed out waiting for account") from e
        except BaseException:
            if acquired:
                self.rate_lock.release()
            raise
        finally:
            if not acquired:
                self.waiting -= 1

    def release_rate_slot(self) -> None:
        if self.rate_lock.locked():
            self.last_fire = time.monotonic()
            self.rate_lock.release()

    @asynccontextmanager
    async def rate_slot(self):
        await self.acquire_rate_slot()
        try:
            yield
        finally:
            self.release_rate_slot()


@dataclass
class ConversationTurn:
    """Generation handle valid inside ``async with pool.turn(thread_id)``."""

    pool: "ConnectionPool"
    thread_id: str
    _responses: list = field(default_factory=list, repr=False)
    _closed: bool = False

    def generate(
        self,
        prompt: str,
        model: str = "",
        stream: bool = False,
        image: bytes | None = None,
        replay_prompt: str | None = None,
        thinking: bool = False,
        search: bool = False,
    ):
        if self._closed:
            raise RuntimeError("This conversation turn has already ended")
        response = self.pool.generate_in_turn(
            self.thread_id,
            prompt,
            model=model,
            stream=stream,
            image=image,
            replay_prompt=replay_prompt,
            thinking=thinking,
            search=search,
        )
        self._responses.append(response)
        return response

    async def _close(self):
        self._closed = True
        try:
            for response in self._responses:
                await response.aclose()
        finally:
            self._responses.clear()


class ConnectionPool:
    def __init__(
        self,
        accounts: list[dict],
        store: StateStore,
        idle_timeout: float = 300.0,
        queue_limit: int = 64,
        queue_timeout: float = 120.0,
        request_delay: float | None = None,
    ):
        if not accounts:
            raise ValueError("No DeepSeek accounts configured")
        if queue_limit < 0 or queue_timeout <= 0:
            raise ValueError("queue_limit must be nonnegative and queue_timeout positive")
        self._conns: list[Connection] = [
            Connection(
                email=a["email"],
                password=a.get("password", ""),
                token=a.get("token"),
                queue_limit=queue_limit,
                queue_timeout=queue_timeout,
                request_delay=max(0.0, REQUEST_DELAY)
                if request_delay is None
                else max(0.0, request_delay),
            )
            for a in accounts
        ]
        self._store = store
        self._idle_timeout = idle_timeout
        self._thread_owner: Dict[str, Connection] = {}  # hot route cache
        self._turn_locks: Dict[str, asyncio.Lock] = {}
        self._turn_owners: dict = {}

    # ---- routing ----
    def _find_conn(self, email: str) -> Optional[Connection]:
        for c in self._conns:
            if c.email == email:
                return c
        return None

    def _assign(self, thread_id: str) -> Connection:
        """Return the connection for a thread, assigning a NEW thread to the
        least-crowded account (fewest active threads within idle TTL)."""
        existing = self._thread_owner.get(thread_id)
        if existing is not None:
            return existing

        # Persisted binding survives restart. Unknown emails (removed/disabled
        # accounts) fall through to fresh assignment so old threads migrate.
        bound_email = self._store.get_binding(thread_id)
        if bound_email is not None:
            conn = self._find_conn(bound_email)
            if conn is not None:
                self._thread_owner[thread_id] = conn
                return conn
            # Stale binding: rebind below.

        # New thread: least crowded among healthy accounts. Accounts in login
        # cooldown (bad password / risky device) are avoided when possible.
        now = time.monotonic()
        healthy = [c for c in self._conns if c.cooldown_until <= now]
        candidates = healthy or list(self._conns)
        occupancy = self._store.occupancy(self._idle_timeout)
        # Ties broken by lowest index (deterministic spread).
        chosen = min(candidates, key=lambda c: (occupancy.get(c.email, 0), self._conns.index(c)))
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
        now = time.monotonic()
        healthy = [c for c in self._conns if c.cooldown_until <= now]
        candidates = healthy or list(self._conns)
        occupancy = self._store.occupancy(self._idle_timeout)
        return min(candidates, key=lambda c: (occupancy.get(c.email, 0), self._conns.index(c)))

    def _failover(self, thread_id: str, bad: Connection) -> Optional[Connection]:
        """Rebind a thread away from a login-dead account. Returns the new
        connection, or None when every account is in cooldown."""
        now = time.monotonic()
        bad.cooldown_until = max(bad.cooldown_until, now + LOGIN_COOLDOWN_S)
        self._thread_owner.pop(thread_id, None)
        healthy = [c for c in self._conns if c is not bad and c.cooldown_until <= now]
        if not healthy:
            return None
        occupancy = self._store.occupancy(self._idle_timeout)
        chosen = min(healthy, key=lambda c: (occupancy.get(c.email, 0), self._conns.index(c)))
        self._store.bind(thread_id, chosen.email)
        self._thread_owner[thread_id] = chosen
        # Owner tuple set by turn(): refresh its connection in place.
        owner = self._turn_owners.get(thread_id)
        if owner is not None:
            self._turn_owners[thread_id] = (owner[0], chosen, owner[2])
        return chosen

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
            async with c.rate_slot():
                c.conversations.clear()
                if c.client is not None:
                    client, c.client = c.client, None
                    await client.__aexit__(None, None, None)

    # ---- conversation + generation (per-account) ----
    async def get_or_create_conversation(self, conn: Connection, thread_id: str) -> Conversation:
        async with conn.rate_slot():
            await conn._ensure_client()
            return self._get_or_create_conversation(conn, thread_id)

    def _get_or_create_conversation(self, conn: Connection, thread_id: str) -> Conversation:
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

    @asynccontextmanager
    async def turn(self, thread_id: str):
        """Hold a conversation across a response and all of its repair calls.

        Consume generators in this task. Context exit closes unfinished output.
        Different conversations may use the account between repair calls.
        """
        owner = self._turn_owners.get(thread_id)
        if owner is not None and owner[0] is asyncio.current_task():
            yield owner[2]
            return
        lock = self._turn_locks.setdefault(thread_id, asyncio.Lock())
        async with lock:
            conn = self.route(thread_id)
            handle = ConversationTurn(self, thread_id)
            self._turn_owners[thread_id] = (asyncio.current_task(), conn, handle)
            try:
                yield handle
            finally:
                try:
                    await handle._close()
                finally:
                    self._turn_owners.pop(thread_id, None)

    async def generate_response(
        self,
        thread_id: str,
        prompt: str,
        model: str = "",
        stream: bool = False,
        image: bytes | None = None,
        replay_prompt: str | None = None,
        thinking: bool = False,
        search: bool = False,
    ):
        """Standalone generation; also safe inside this task's existing turn."""
        async with self.turn(thread_id):
            async with aclosing(
                self.generate_in_turn(
                    thread_id,
                    prompt,
                    model=model,
                    stream=stream,
                    image=image,
                    replay_prompt=replay_prompt,
                    thinking=thinking,
                    search=search,
                )
            ) as response:
                async for chunk in response:
                    yield chunk

    async def generate_in_turn(
        self,
        thread_id: str,
        prompt: str,
        model: str = "",
        stream: bool = False,
        image: bytes | None = None,
        replay_prompt: str | None = None,
        thinking: bool = False,
        search: bool = False,
    ):
        owner = self._turn_owners.get(thread_id)
        if owner is None or owner[0] is not asyncio.current_task():
            raise RuntimeError("generate_in_turn requires an active turn(thread_id) in this task")
        async with aclosing(
            self._generate_response_unlocked(
                thread_id,
                prompt,
                model=model,
                stream=stream,
                image=image,
                conn=owner[1],
                replay_prompt=replay_prompt,
                thinking=thinking,
                search=search,
            )
        ) as response:
            async for chunk in response:
                yield chunk

    async def _generate_response_unlocked(
        self,
        thread_id: str,
        prompt: str,
        model: str = "",
        stream: bool = False,
        image: bytes | None = None,
        *,
        conn: Connection,
        replay_prompt: str | None = None,
        thinking: bool = False,
        search: bool = False,
    ):
        """Generate one turn; caller holds the per-thread generation lock.

        Retries rate-limited / expired-token turns before yielding output.

        Rate-limit retries happen BEFORE the first byte (safe for both
        stream and non-stream callers); a mid-stream failure still raises
        immediately since already-yielded chunks can't be taken back.
        Exhausted rate limits raise BackendRateLimited (mapped to HTTP 429).
        """
        max_retries = max(0, int(RATE_LIMIT_MAX_RETRIES))
        backoff = max(0.5, float(RATE_LIMIT_BACKOFF_S))
        refreshed = False
        attempt = 0
        yielded = False
        while True:
            async with conn.rate_slot():
                try:
                    async with aclosing(
                        self._generate_once(
                            conn,
                            thread_id,
                            prompt,
                            model=model,
                            stream=stream,
                            image=image,
                            replay_prompt=replay_prompt,
                            thinking=thinking,
                            search=search,
                        )
                    ) as response:
                        async for chunk in response:
                            yielded = True
                            yield chunk
                    return
                except DeepSeekError as e:
                    if _is_login_error(e) and not yielded and not thread_id.startswith("title_"):
                        log.error(
                            "DeepSeek login dead, failing over: %r",
                            e,
                            extra={"thread_id": thread_id},
                        )
                        nxt = self._failover(thread_id, conn)
                        if nxt is not None:
                            conn = nxt
                            refreshed = False
                            continue
                    if _is_invalid_token_error(e) and not refreshed and not yielded:
                        refreshed = True
                        await conn._reconnect()
                        continue
                    if _is_rate_limit_error(e):
                        wait = backoff * min(attempt + 1, max(1, max_retries))
                        conn.cooldown_until = max(conn.cooldown_until, time.monotonic() + wait)
                        if not yielded and attempt < max_retries:
                            attempt += 1
                            log.info(
                                "Retrying DeepSeek request after transient backend error",
                                extra={
                                    "thread_id": thread_id,
                                    "attempt": attempt,
                                    "retry_delay_s": wait,
                                },
                            )
                            continue
                        if not yielded:
                            raise BackendRateLimited(
                                f"DeepSeek backend rate-limited this turn ({attempt} retries exhausted)",
                                retry_after=wait,
                            ) from e
                    raise

    async def _generate_once(
        self,
        conn: Connection,
        thread_id: str,
        prompt: str,
        model: str = "",
        stream: bool = False,
        image: bytes | None = None,
        replay_prompt: str | None = None,
        thinking: bool = False,
        search: bool = False,
    ):
        # Caller holds the account gate, including initialization and cleanup.
        log.debug(
            "DeepSeek request",
            extra={"thread_id": thread_id, "model": model, "stream": stream},
        )
        await conn._ensure_client()
        conv = self._get_or_create_conversation(conn, thread_id)
        model_type = _resolve_model(model)
        saved_session = conn.client._session_id
        session_id = conn.thread_sessions.get(thread_id)
        uploaded = None
        yielded = False
        try:
            if session_id is None:
                session_id = await conn.client.create_chat_session(conn.client._token)
                conn.thread_sessions[thread_id] = session_id
                log.debug(
                    "New remote session",
                    extra={"thread_id": thread_id},
                )
            conn.client._session_id = session_id

            if image:
                try:
                    uploaded = await conn.client.upload_image(image)
                    log.debug("Image uploaded: file_id=%s", uploaded.file_id)
                except Exception as e:
                    if _is_rate_limit_error(e) or _is_invalid_token_error(e):
                        raise
                    log.warning("Image upload failed, continuing text-only: %s", e)

            if stream:
                from .thinking import stream_reasoning

                async with aclosing(stream_reasoning(
                    conv.ask_stream(prompt, image=uploaded, model=model_type),
                    thinking=thinking, search=search,
                )) as response:
                    async for chunk in response:
                        yielded = True
                        yield chunk
            else:
                from .thinking import Reasoning, reasoning_capture, request_flags

                with request_flags(thinking, search), reasoning_capture() as sink:
                    response = await conv.ask(prompt, image=uploaded, model=model_type)
                if sink:
                    yield Reasoning("".join(sink))
                yield response.text
        except DeepSeekError as e:
            log.error(
                "DeepSeek API error [%s]: %s",
                type(e).__name__,
                repr(e),
                extra={"thread_id": thread_id},
            )
            if "input_exceeds_limit" in str(e).lower() and not yielded:
                if not replay_prompt:
                    raise
                log.warning(
                    "DeepSeek context limit reached; rebuilding thread=%s in a new remote session",
                    thread_id,
                )
                await asyncio.sleep(conn.request_delay)
                session_id = await conn.client.create_chat_session(conn.client._token)
                with conn.map_lock:
                    conn.thread_sessions[thread_id] = session_id
                    conv = conn.client.new_conversation()
                    conn.conversations[thread_id] = conv
                conn.client._session_id = session_id
                prompt = replay_prompt
                if stream:
                    from .thinking import stream_reasoning

                    async with aclosing(stream_reasoning(
                        conv.ask_stream(prompt, image=uploaded, model=model_type),
                        thinking=thinking, search=search,
                    )) as response:
                        async for chunk in response:
                            yielded = True
                            yield chunk
                else:
                    from .thinking import Reasoning, reasoning_capture, request_flags

                    with request_flags(thinking, search), reasoning_capture() as sink:
                        response = await conv.ask(prompt, image=uploaded, model=model_type)
                    if sink:
                        yield Reasoning("".join(sink))
                    yield response.text
                return
            raise
        finally:
            conn.client._session_id = saved_session
            if session_id and conv.parent_message_id:
                self._save_resume(conn, thread_id, session_id, conv.parent_message_id)

    def _save_resume(
        self, conn: Connection, thread_id: str, session_id: str, parent_message_id: str
    ) -> None:
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


def init_pool(
    db_path: str,
    idle_timeout: float = 300.0,
    snapshot_interval: float = 5.0,
    queue_limit: int = 64,
    queue_timeout: float = 120.0,
    request_delay: float | None = None,
    storage_backend: str = "sqlite",
    mongo_uri: str = "",
    mongo_db: str = "deepseek_proxy",
) -> ConnectionPool:
    global _pool
    backend = build_backend(
        storage_backend, db_path=db_path, mongo_uri=mongo_uri, mongo_db=mongo_db
    )
    store = StateStore(db_path, snapshot_interval=snapshot_interval, backend=backend)
    store.open()
    _pool = ConnectionPool(
        ACCOUNTS,
        store,
        idle_timeout=idle_timeout,
        queue_limit=queue_limit,
        queue_timeout=queue_timeout,
        request_delay=request_delay,
    )
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


async def generate_response(
    thread_id: str,
    prompt: str,
    model: str = "",
    stream: bool = False,
    image: bytes | None = None,
    replay_prompt: str | None = None,
    thinking: bool = False,
    search: bool = False,
):
    pool = get_pool()
    async with aclosing(
        pool.generate_response(
            thread_id,
            prompt,
            model=model,
            stream=stream,
            image=image,
            replay_prompt=replay_prompt,
            thinking=thinking,
            search=search,
        )
    ) as response:
        async for chunk in response:
            yield chunk


def turn(thread_id: str):
    return get_pool().turn(thread_id)


async def generate_in_turn(
    thread_id: str,
    prompt: str,
    model: str = "",
    stream: bool = False,
    image: bytes | None = None,
    replay_prompt: str | None = None,
    thinking: bool = False,
    search: bool = False,
):
    async with aclosing(
        get_pool().generate_in_turn(
            thread_id,
            prompt,
            model=model,
            stream=stream,
            image=image,
            replay_prompt=replay_prompt,
            thinking=thinking,
            search=search,
        )
    ) as response:
        async for chunk in response:
            yield chunk


def get_thread_tokens(thread_id: str) -> int:
    return get_pool().get_thread_tokens(thread_id)


def add_thread_tokens(thread_id: str, n: int) -> None:
    get_pool().add_thread_tokens(thread_id, n)


def get_thread_exchanges(thread_id: str) -> int:
    return get_pool().get_thread_exchanges(thread_id)


def bump_thread_exchanges(thread_id: str, n: int) -> None:
    get_pool().bump_thread_exchanges(thread_id, n)
