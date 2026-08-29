import json
import logging
import time
import uuid
from collections.abc import AsyncGenerator
from typing import Any, Dict, List, Optional

from config import TOOL_TAG_CLOSE, TOOL_TAG_OPEN, estimate_tokens
from deepseek_client import add_thread_tokens
from tool_parser import (
    ESC_TO_SENTINEL,
    MAX_HEADER_LEN,
    build_tool_call,
    extract_name_from_header,
)

log = logging.getLogger(__name__)

OPEN = TOOL_TAG_OPEN
CLOSE = TOOL_TAG_CLOSE
TRIGGER = OPEN[0] if OPEN else None

TEXT, PEEK, HEADER, TOOL, ESC = "TEXT", "PEEK", "HEADER", "TOOL", "ESC"
ESC_MARKERS = list(ESC_TO_SENTINEL)


def _is_prefix(s: str, target: str) -> bool:
    return target.startswith(s)


def _backtrack(s: str, target: str) -> tuple[str, str]:
    """Return (flushed, keep) — longest suffix of s that is a prefix of target."""
    for i in range(1, len(s)):
        suffix = s[i:]
        if target.startswith(suffix):
            return s[:i], suffix
    return s, ""


def format_sse(data: Dict[str, Any]) -> str:
    return f"data: {json.dumps(data, ensure_ascii=False)}\n\n"


def _content_delta(chunk_id: str, created: int, model: str, c: str) -> str:
    return format_sse(
        {
            "id": chunk_id,
            "object": "chat.completion.chunk",
            "created": created,
            "model": model,
            "choices": [{"index": 0, "delta": {"content": c}, "finish_reason": None}],
        }
    )


def _tool_call_delta(
    chunk_id: str, created: int, model: str, idx: int, tc: Dict[str, Any]
) -> str:
    return format_sse(
        {
            "id": chunk_id,
            "object": "chat.completion.chunk",
            "created": created,
            "model": model,
            "choices": [
                {
                    "index": 0,
                    "delta": {
                        "role": "assistant",
                        "content": None,
                        "tool_calls": [{"index": idx, **tc}],
                    },
                    "finish_reason": None,
                }
            ],
        }
    )


def _finish_chunk(chunk_id, created, model, reason, usage) -> str:
    return format_sse(
        {
            "id": chunk_id,
            "object": "chat.completion.chunk",
            "created": created,
            "model": model,
            "choices": [{"index": 0, "delta": {}, "finish_reason": reason}],
            "usage": usage,
        }
    )


async def hybrid_stream_generator(
    response_gen: AsyncGenerator[str, None],
    model: str,
    thread_id: str,
    prompt_tokens: int,
    turn_prompt: int,
) -> AsyncGenerator[str, None]:
    state = TEXT
    esc_from = TEXT
    esc_buf = ""
    peek_buf = ""
    header_buf = ""
    json_buf = ""
    pending_name: Optional[str] = None
    chunk_id = f"chatcmpl-{uuid.uuid4().hex}"
    created = int(time.time())
    tool_calls: List[Dict[str, Any]] = []
    text_buf = ""  # accounting only; text chars are yielded as they arrive

    try:
        async for chunk in response_gen:
            for char in chunk:
                if TRIGGER is None:
                    text_buf += char
                    yield _content_delta(chunk_id, created, model, char)
                    continue

                if state == ESC:
                    cand = esc_buf + char
                    if cand in ESC_TO_SENTINEL:
                        if esc_from == TOOL:
                            json_buf += ESC_TO_SENTINEL[cand]
                        else:
                            yield _content_delta(chunk_id, created, model, cand[1:])
                        state = esc_from
                        esc_buf = ""
                    elif any(m.startswith(cand) for m in ESC_MARKERS):
                        esc_buf = cand
                    elif esc_buf == "\\" and char == "\\":
                        if esc_from == TOOL:
                            json_buf += "\\"
                        else:
                            yield _content_delta(chunk_id, created, model, "\\")
                        esc_buf = "\\"  # stay armed for the next char
                    else:
                        literal = esc_buf + char
                        if esc_from == TOOL:
                            json_buf += literal
                        else:
                            yield _content_delta(chunk_id, created, model, literal)
                        state = esc_from
                        esc_buf = ""

                elif state == TEXT:
                    if char == "\\":
                        esc_from, esc_buf, state = TEXT, "\\", ESC
                    elif char == TRIGGER:
                        state = PEEK
                        peek_buf = char
                    else:
                        text_buf += char
                        yield _content_delta(chunk_id, created, model, char)

                elif state == PEEK:
                    peek_buf += char
                    if peek_buf == OPEN:
                        state = HEADER
                        header_buf = ""
                    elif not _is_prefix(peek_buf, OPEN):
                        flushed, keep = _backtrack(peek_buf, OPEN)
                        for c in flushed:
                            text_buf += c
                            yield _content_delta(chunk_id, created, model, c)
                        peek_buf = keep
                        if not peek_buf:
                            state = TEXT

                elif state == HEADER:
                    header_buf += char
                    if char == ">":
                        pending_name = extract_name_from_header(header_buf)
                        state = TOOL
                        json_buf = ""
                    elif len(header_buf) > MAX_HEADER_LEN:
                        log.warning(
                            "Oversized <invoke header (%d chars) — flushing as text",
                            len(header_buf),
                        )
                        for c in header_buf:
                            text_buf += c
                            yield _content_delta(chunk_id, created, model, c)
                        header_buf = ""
                        state = TEXT

                elif state == TOOL:
                    if char == "\\":
                        esc_from, esc_buf, state = TOOL, "\\", ESC
                    else:
                        json_buf += char
                        if CLOSE and json_buf.endswith(CLOSE):
                            body = json_buf[: -len(CLOSE)]
                            tc = build_tool_call(pending_name, body)
                            if tc:
                                tool_calls.append(tc)
                                idx = len(tool_calls) - 1
                                yield _tool_call_delta(
                                    chunk_id, created, model, idx, tc
                                )
                            else:
                                log.warning(
                                    "Dropped unparseable <invoke> payload: %.300r",
                                    body,
                                )
                            state = TEXT
                            peek_buf = ""
                            header_buf = ""
                            json_buf = ""
                            pending_name = None

        # ---- Stream ended ----
        if state == ESC:
            literal = esc_buf
            if esc_from == TOOL:
                json_buf += literal
                state = TOOL
            else:
                state = TEXT
            esc_buf = ""
            # fall through to the matching EOF branch below

        if state == TOOL and json_buf:
            # Truncated mid-call: salvage via tolerant parse, else drop —
            # never leak raw JSON as chat text.
            try:
                tc = build_tool_call(pending_name, json_buf)
            except Exception:
                tc = None
            if tc:
                log.warning("Recovered truncated <invoke> block at EOF")
                tool_calls.append(tc)
                idx = len(tool_calls) - 1
                yield _tool_call_delta(chunk_id, created, model, idx, tc)
            else:
                log.warning("Dropped truncated tool call at EOF: %.300r", json_buf)
            json_buf = ""
            pending_name = None

        elif state == HEADER:
            # Consumed OPEN but never saw '>' — flush everything back as text.
            log.warning("Truncated <invoke header at EOF: %.200r", header_buf)
            for c in OPEN + header_buf:
                text_buf += c
                yield _content_delta(chunk_id, created, model, c)
            header_buf = ""

        elif state == PEEK and peek_buf:
            for c in peek_buf:
                text_buf += c
                yield _content_delta(chunk_id, created, model, c)
            peek_buf = ""

        elif state == TEXT and esc_buf:
            for c in esc_buf:
                text_buf += c
                yield _content_delta(chunk_id, created, model, c)
            esc_buf = ""

        if tool_calls:
            all_args = json.dumps([tc["function"]["arguments"] for tc in tool_calls])
            completion_tokens = estimate_tokens(all_args)
            add_thread_tokens(thread_id, turn_prompt + completion_tokens)
            usage = {
                "prompt_tokens": prompt_tokens,
                "completion_tokens": completion_tokens,
                "total_tokens": prompt_tokens + completion_tokens,
            }
            yield _finish_chunk(chunk_id, created, model, "tool_calls", usage)
            yield "data: [DONE]\n\n"
            return

        completion_tokens = estimate_tokens(text_buf)
        add_thread_tokens(thread_id, turn_prompt + completion_tokens)
        usage = {
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens,
            "total_tokens": prompt_tokens + completion_tokens,
        }
        yield _finish_chunk(chunk_id, created, model, "stop", usage)
        yield "data: [DONE]\n\n"

    except Exception as e:
        log.error("Stream error [%s]: %s", type(e).__name__, repr(e))
        # Text was already yielded char-by-char — flush only UN-yielded buffers.
        leftover = ""
        if state == PEEK:
            leftover = peek_buf
        elif state == HEADER:
            leftover = OPEN + header_buf
        elif state == ESC:
            leftover = esc_buf if esc_from == TEXT else json_buf + esc_buf
        elif state == TOOL:
            leftover = json_buf
        if leftover:
            yield format_sse(
                {
                    "id": chunk_id,
                    "object": "chat.completion.chunk",
                    "created": created,
                    "model": model,
                    "choices": [
                        {
                            "index": 0,
                            "delta": {"content": leftover},
                            "finish_reason": "stop",
                        }
                    ],
                }
            )
        else:
            yield format_sse(
                {
                    "id": chunk_id,
                    "object": "chat.completion.chunk",
                    "created": created,
                    "model": model,
                    "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                }
            )
        yield "data: [DONE]\n\n"
