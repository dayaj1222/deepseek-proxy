"""Opt-in "thinking"/"search" mode patch for the installed aiodeepseek client.

aiodeepseek==0.1.1 hard-codes ``thinking_enabled=False`` and
``search_enabled=False`` in the request body built by
:meth:`_ChatClient.stream_chat`, and its ``_extract_fragment`` flattens every
fragment (THINK, RESPONSE, SEARCH) into a single text stream. We do not fork or
edit the installed library; this module replaces ``stream_chat`` with an
equivalent SSE loop from our side at proxy startup.

Both ``Conversation.ask`` and ``Conversation.ask_stream`` funnel through
``_ChatClient.stream_chat`` (see ``aiodeepseek/conversation.py``), so patching
that single method covers every generation path in this proxy.

Pinned to aiodeepseek==0.1.1. If the library is upgraded, re-verify that
``stream_chat`` still builds the same body and that the helpers imported below
(``_build_pow_header``, ``_effective_timeout``, ``_aiohttp_timeout``) and the
constants (``BASE_URL``, ``COMPLETION_PATH``, ``HEADERS``) still exist.

Fragment protocol (verified live against chat.deepseek.com
/api/v0/chat/completion):

  - The initial bulk object declares fragments as a list of
    ``{id, type, content}`` where ``type`` is ``THINK``, ``RESPONSE`` or
    ``SEARCH``.
  - ``{"o": "APPEND", "p": "response/fragments", "v": [{id, type, content}]}``
    declares a NEW fragment and makes it the current one.
  - Content chunks (``{"v": "..."}`` or
    ``{"o": "APPEND", "p": "response/fragments/-1/content", "v": "..."}``)
    belong to the most recently declared fragment.
  - A ``SEARCH`` fragment may be followed by
    ``{"p": "response/fragments/-1/results", "v": [...]}`` result lists.
  - The stream ends with a BATCH quasi_status FINISHED, then
    ``SET response/status FINISHED``.

Routing rules implemented here:

  - THINK content -> reasoning channel (never into the text stream, so it is
    never seen by ToolParser / tool-call extraction / token counting).
  - RESPONSE content -> unchanged text channel, ``[citation:N]`` markers
    preserved verbatim.
  - SEARCH fragment text and result lists -> dropped.

When both flags are off the request body carries false flags and normal response
text follows the same extraction path as upstream.
"""

from __future__ import annotations

import asyncio
import inspect
import json
import logging
from contextlib import aclosing, contextmanager, suppress
from contextvars import ContextVar
from dataclasses import dataclass
from typing import Callable, Dict, List, Optional

from aiodeepseek.data.constants import BASE_URL, COMPLETION_PATH, HEADERS
from aiodeepseek.http._config import _DEV_MODE
from aiodeepseek.http._sse import _coerce_message_id, _extract_message_id
from aiodeepseek.log import _log, _log_request
from aiodeepseek.types.exceptions import DeepSeekError, raise_for_sse_hint

log = logging.getLogger(__name__)


@dataclass
class Reasoning:
    """Internal event carrying captured thinking text up to the transport."""

    text: str


# Per-call sink for reasoning text. Our backend sets this around the
# ``conv.ask_stream(...)`` iteration; the patched ``stream_chat`` appends THINK
# fragments here. Kept in a ContextVar so concurrent turns never interleave.
_reasoning_sink: ContextVar[Optional[List[str]]] = ContextVar("reasoning_sink", default=None)
_reasoning_callback: ContextVar[Optional[Callable[[str], object]]] = ContextVar(
    "reasoning_callback", default=None
)


@contextmanager
def reasoning_capture(on_piece=None, *, collect=True):
    """Collect reasoning text emitted during one generation call.

    Yields a list that the patched ``stream_chat`` appends THINK fragments to.
    Outside this context the patch simply discards reasoning (it is never part
    of the text stream).
    """
    sink: List[str] = []
    token = _reasoning_sink.set(sink if collect else None)
    callback_token = _reasoning_callback.set(on_piece)
    try:
        yield sink
    finally:
        _reasoning_callback.reset(callback_token)
        _reasoning_sink.reset(token)


def _append_reasoning(text: str) -> object | None:
    sink = _reasoning_sink.get()
    if sink is not None and text:
        sink.append(text)
    callback = _reasoning_callback.get()
    if callback is not None and text:
        return callback(text)


def _fragment_type(fragment) -> str:
    if isinstance(fragment, dict):
        return str(fragment.get("type", "")).upper()
    return ""


def _fragment_content(fragment) -> str:
    if isinstance(fragment, dict):
        content = fragment.get("content")
        if isinstance(content, str):
            return content
    return ""


class _FragmentRouter:
    """Tracks fragment declarations and routes content by fragment type.

    FRAGMENT TYPE DECISION POINT: THINK -> reasoning channel, RESPONSE -> text
    channel, SEARCH -> dropped. This is the single place the proxy decides what
    a fragment's content means.
    """

    def __init__(self) -> None:
        # Stack of declared fragment types; index -1 is the current fragment.
        # ``None`` means "no fragment declared yet" (chunks go to text).
        self._types: List[Optional[str]] = []
        self._last_was_batch = False

    def declare(self, fragments) -> None:
        """Handle a fragment declaration list (initial bulk or APPEND list)."""
        if not isinstance(fragments, list):
            return
        for fragment in fragments:
            self._types.append(_fragment_type(fragment) or None)

    def _current(self) -> Optional[str]:
        if not self._types:
            return None
        return self._types[-1]

    def route_content(self, text: str) -> Optional[str]:
        """Return the channel name for *text*: "text", "reasoning", or None."""
        if not text:
            return None
        current = self._current()
        if current == "THINK":
            return "reasoning"
        if current == "SEARCH":
            return None
        return "text"


def _looks_like_fragment_list(value) -> bool:
    return (
        isinstance(value, list)
        and len(value) > 0
        and all(isinstance(item, dict) and ("type" in item or "id" in item) for item in value)
    )


def _route_event(event, router: _FragmentRouter):
    """Yield ``(channel, text)`` pairs from one parsed SSE event.

    Channels: ``"text"`` (RESPONSE), ``"reasoning"`` (THINK). SEARCH content is
    dropped. Only fragment content is emitted; metadata is not answer text.
    """
    if not isinstance(event, dict):
        return
    o = event.get("o", "")
    p = event.get("p", "")
    v = event.get("v")

    # BATCH paths are relative to the enclosing patch. Process them in wire
    # order: a fragment declaration changes the channel for later deltas.
    if o == "BATCH" and isinstance(v, list):
        for patch in v:
            if not isinstance(patch, dict):
                continue
            child = dict(patch)
            child_path = child.get("p", "")
            child["p"] = "/".join(part.strip("/") for part in (p, child_path) if part)
            yield from _route_event(child, router)
        return

    # Declaration: initial bulk object carries fragments inside v.response.
    # Inline content on declared fragments is routed by type, mirroring the
    # stock extractor (which yields bulk content as text): RESPONSE -> text,
    # THINK -> reasoning, SEARCH -> dropped.
    if isinstance(v, dict) and "response" in v:
        fragments = v["response"].get("fragments", [])
        if isinstance(fragments, list):
            router.declare(fragments)
            for fragment in fragments:
                if not isinstance(fragment, dict):
                    continue
                ftype = _fragment_type(fragment)
                content = _fragment_content(fragment)
                if not content:
                    continue
                if ftype == "THINK":
                    yield ("reasoning", content)
                elif ftype == "SEARCH":
                    continue
                else:
                    yield ("text", content)
        return

    # Declaration: APPEND to response/fragments with a new fragment list.
    # The declaration includes the first token; subsequent deltas do not repeat
    # it. Route that token immediately, after updating the fragment channel.
    if o == "APPEND" and p == "response/fragments" and _looks_like_fragment_list(v):
        for fragment in v:
            router.declare([fragment])
            content = _fragment_content(fragment)
            channel = router.route_content(content)
            if channel:
                yield channel, content
        return

    # Search results lists are dropped (they never contain reply text).
    if isinstance(p, str) and p.endswith("/results"):
        return

    # Content chunk for the current fragment.
    is_text_delta = isinstance(v, str) and (
        (not o and not p) or (o in ("", "APPEND") and p == "response/fragments/-1/content")
    )
    if is_text_delta:
        channel = router.route_content(v)
        if channel == "reasoning":
            yield ("reasoning", v)
        elif channel == "text":
            yield ("text", v)
        return

    # Metadata and unknown patch paths must never be interpreted as text.


async def _routed_stream_chat(
    self,
    token: str,
    session_id: str,
    prompt: str,
    model,
    timeout=None,
    parent_message_id=None,
    image=None,
    cumulative: bool = True,
):
    """Drop-in replacement for ``_ChatClient.stream_chat`` with fragment routing.

    Same ``(text, message_id)`` yield contract as upstream for RESPONSE text;
    THINK fragments are diverted to the reasoning sink and never yielded.
    """
    assert self._session is not None, "Session not started"

    from aiodeepseek.types.enums import ModelType

    pow_header = await self._build_pow_header(token, COMPLETION_PATH, timeout)
    effective = self._effective_timeout(timeout)

    headers: Dict[str, str] = {
        **HEADERS,
        "Authorization": f"Bearer {token}",
        "Accept": "text/event-stream",
        "X-DS-PoW-Response": pow_header,
    }

    body: Dict = {
        "chat_session_id": session_id,
        "parent_message_id": _coerce_message_id(parent_message_id),
        "prompt": prompt,
        "ref_file_ids": [image.file_id] if image is not None else [],
        "thinking_enabled": bool(_thinking_on.get()),
        "search_enabled": bool(_search_on.get()),
        "audio_id": None,
        "preempt": False,
        "model_type": model if model is not None else ModelType.DEFAULT,
    }

    _log_request("STREAM CHAT REQUEST", BASE_URL + COMPLETION_PATH, headers, body)

    accumulated: str = ""
    message_id: Optional[str] = None
    router = _FragmentRouter()

    async with self._session.post(
        BASE_URL + COMPLETION_PATH,
        json=body,
        headers=headers,
        timeout=self._aiohttp_timeout(effective),
    ) as resp:
        if _DEV_MODE:
            _log.debug("<<< STREAM CHAT RESPONSE  status=%s", resp.status)

        if resp.status != 200:
            raw = await resp.text()
            raise DeepSeekError(f"HTTP {resp.status}: {raw[:400]}", resp.status)

        current_event: str = "message"

        async for raw_line in resp.content:
            line: str = raw_line.decode("utf-8", errors="replace").strip()
            if not line:
                continue
            if line.startswith("event:"):
                current_event = line[6:].strip()
                continue
            if not line.startswith("data:"):
                current_event = "message"
                continue
            data_str: str = line[5:].strip()
            if not data_str or data_str == "[DONE]":
                current_event = "message"
                continue
            try:
                event = json.loads(data_str)
            except json.JSONDecodeError:
                current_event = "message"
                continue

            if current_event == "hint" and isinstance(event, dict) and event.get("type") == "error":
                raise_for_sse_hint(event)
            current_event = "message"

            if message_id is None:
                new_mid = _extract_message_id(event)
                if new_mid is not None:
                    message_id = new_mid

            for channel, piece in _route_event(event, router):
                if channel == "reasoning":
                    pending = _append_reasoning(piece)
                    if inspect.isawaitable(pending):
                        await pending
                    continue
                accumulated += piece
                if cumulative:
                    yield accumulated, message_id
                else:
                    yield piece, message_id


# Per-call toggles set by the backend around each generation. ContextVars keep
# concurrent turns isolated.
_thinking_on: ContextVar[bool] = ContextVar("thinking_on", default=False)
_search_on: ContextVar[bool] = ContextVar("search_on", default=False)


@contextmanager
def request_flags(thinking: bool, search: bool):
    """Set the per-request thinking/search flags for the patched stream_chat."""
    t = _thinking_on.set(bool(thinking))
    s = _search_on.set(bool(search))
    try:
        yield
    finally:
        _search_on.reset(s)
        _thinking_on.reset(t)


async def stream_reasoning(source, *, thinking: bool, search: bool):
    """Merge thinking and answer deltas in wire order with bounded buffering.

    The producer owns the capture context and upstream iterator. Cancelling or
    closing this stream closes the upstream HTTP response without leaving tasks.
    """
    queue = asyncio.Queue(maxsize=32)

    async def publish_reasoning(piece):
        await queue.put(("reasoning", piece))

    async def produce():
        try:
            with (
                request_flags(thinking, search),
                reasoning_capture(publish_reasoning, collect=False),
            ):
                async with aclosing(source):
                    async for text in source:
                        await queue.put(("text", text))
        except Exception as exc:
            await queue.put(("error", exc))
        finally:
            if not asyncio.current_task().cancelling():
                await queue.put(("done", None))

    producer = asyncio.create_task(produce())
    try:
        while True:
            kind, value = await queue.get()
            if kind == "done":
                break
            if kind == "error":
                raise value
            yield Reasoning(value) if kind == "reasoning" else value
    finally:
        producer.cancel()
        with suppress(asyncio.CancelledError):
            await producer


_APPLIED = False


def apply(enabled: bool) -> bool:
    """Install the routed ``stream_chat``. Idempotent.

    Always install the router so per-request thinking works even when its
    global setting is off. Unlike the stock extractor, the router also preserves
    inline tokens in APPEND declarations and recognizes nested BATCH patches.
    """
    global _APPLIED
    if _APPLIED:
        return True

    from aiodeepseek.clients import chat as chat_client

    chat_client._ChatClient.stream_chat = _routed_stream_chat

    _APPLIED = True
    log.info("Thinking/search mode enabled: aiodeepseek stream_chat patched")
    return True


def is_applied() -> bool:
    return _APPLIED


__all__ = [
    "Reasoning",
    "apply",
    "is_applied",
    "reasoning_capture",
    "request_flags",
    "stream_reasoning",
]
