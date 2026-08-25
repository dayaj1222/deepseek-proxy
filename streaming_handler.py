import json
import logging
import time
import uuid
from typing import Any, AsyncGenerator, Dict, List

from config import estimate_tokens
from deepseek_client import add_thread_tokens
from tool_parser import parse_tool_call_json


log = logging.getLogger(__name__)

PREFIX = "[TOOL CALL]"
SUFFIX = "[/TOOL CALL]"


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


async def hybrid_stream_generator(
    response_gen: AsyncGenerator[str, None],
    model: str,
    thread_id: str,
    prompt_tokens: int,
    turn_prompt: int,
) -> AsyncGenerator[str, None]:
    state = "TEXT"
    peek_buf = ""
    json_buf = ""
    chunk_id = f"chatcmpl-{uuid.uuid4().hex}"
    created = int(time.time())
    tool_calls: List[Dict[str, Any]] = []
    text_buf = ""

    try:
        async for chunk in response_gen:
            for char in chunk:
                if state == "TEXT":
                    if char == "[":
                        state = "PEEK"
                        peek_buf = "["
                    else:
                        text_buf += char
                        yield format_sse({
                            "id": chunk_id, "object": "chat.completion.chunk",
                            "created": created, "model": model,
                            "choices": [{"index": 0, "delta": {"content": char}, "finish_reason": None}],
                        })

                elif state == "PEEK":
                    peek_buf += char
                    if peek_buf == PREFIX:
                        state = "TOOL"
                        json_buf = ""
                    elif not _is_prefix(peek_buf, PREFIX):
                        flushed, keep = _backtrack(peek_buf, PREFIX)
                        for c in flushed:
                            text_buf += c
                            yield format_sse({
                                "id": chunk_id, "object": "chat.completion.chunk",
                                "created": created, "model": model,
                                "choices": [{"index": 0, "delta": {"content": c}, "finish_reason": None}],
                            })
                        peek_buf = keep
                        if not peek_buf:
                            state = "TEXT"

                elif state == "TOOL":
                    json_buf += char
                    if json_buf.endswith(SUFFIX):
                        content = json_buf[:-len(SUFFIX)]
                        try:
                            tc = parse_tool_call_json(content)
                            if tc:
                                tool_calls.append(tc)
                                idx = len(tool_calls) - 1
                                yield format_sse({
                                    "id": chunk_id, "object": "chat.completion.chunk",
                                    "created": created, "model": model,
                                    "choices": [{"index": 0, "delta": {
                                        "role": "assistant", "content": None,
                                        "tool_calls": [{"index": idx, **tc}],
                                    }, "finish_reason": None}],
                                })
                        except Exception:
                            log.exception("Failed to parse tool call JSON")
                        state = "TEXT"
                        peek_buf = ""
                        json_buf = ""

        # Stream ended
        if state == "TOOL" and json_buf:
            # Truncated mid-call: salvage via repair, emit as tool_call,
            # otherwise drop silently — never leak raw JSON as chat text.
            try:
                tc = parse_tool_call_json(json_buf)
            except Exception:
                tc = None
            if tc:
                tool_calls.append(tc)
                idx = len(tool_calls) - 1
                yield format_sse({
                    "id": chunk_id, "object": "chat.completion.chunk",
                    "created": created, "model": model,
                    "choices": [{"index": 0, "delta": {
                        "role": "assistant", "content": None,
                        "tool_calls": [{"index": idx, **tc}],
                    }, "finish_reason": None}],
                })
            else:
                log.warning("Dropped unparseable truncated tool call (%d chars)", len(json_buf))
            json_buf = ""

        if tool_calls:
            all_args = json.dumps([tc["function"]["arguments"] for tc in tool_calls])
            completion_tokens = estimate_tokens(all_args)
            add_thread_tokens(thread_id, turn_prompt + completion_tokens)
            yield format_sse({
                "id": chunk_id, "object": "chat.completion.chunk",
                "created": created, "model": model,
                "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}],
                "usage": {
                    "prompt_tokens": prompt_tokens,
                    "completion_tokens": completion_tokens,
                    "total_tokens": prompt_tokens + completion_tokens,
                },
            })
            yield "data: [DONE]\n\n"
            return

        if state == "TOOL":
            text_buf += json_buf
        elif state == "PEEK":
            for c in peek_buf:
                text_buf += c
                yield format_sse({
                    "id": chunk_id, "object": "chat.completion.chunk",
                    "created": created, "model": model,
                    "choices": [{"index": 0, "delta": {"content": c}, "finish_reason": None}],
                })

        completion_tokens = estimate_tokens(text_buf)
        add_thread_tokens(thread_id, turn_prompt + completion_tokens)
        yield format_sse({
            "id": chunk_id, "object": "chat.completion.chunk",
            "created": created, "model": model,
            "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
            "usage": {
                "prompt_tokens": prompt_tokens,
                "completion_tokens": completion_tokens,
                "total_tokens": prompt_tokens + completion_tokens,
            },
        })
        yield "data: [DONE]\n\n"

    except Exception as e:
        log.error("Stream error [%s]: %s", type(e).__name__, repr(e))
        yield format_sse({
            "id": chunk_id, "object": "chat.completion.chunk",
            "created": created, "model": model,
            "choices": [{
                "index": 0,
                "delta": {"content": text_buf + json_buf} if (text_buf or json_buf) else {},
                "finish_reason": "stop",
            }],
        })
        yield "data: [DONE]\n\n"
        return
