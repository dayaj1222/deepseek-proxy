import json
import logging
import time
import uuid
from typing import Any, AsyncGenerator, Dict, List

from config import TOOL_BUFFER_LIMIT, estimate_tokens
from tool_parser import extract_tool_calls


log = logging.getLogger(__name__)

TOOL_MARKER = "⟿"
TEXT_FLUSH_THRESHOLD = 200


def format_sse(data: Dict[str, Any]) -> str:
    return f"data: {json.dumps(data, ensure_ascii=False)}\n\n"


def _has_complete_tool_call(buffer: str) -> bool:
    calls = extract_tool_calls(buffer)
    return len(calls) > 0


async def hybrid_stream_generator(
    response_gen: AsyncGenerator[str, None],
    model: str,
    thread_id: str,
    prompt_tokens: int,
) -> AsyncGenerator[str, None]:
    buffer = ""
    chunk_id = f"chatcmpl-{uuid.uuid4().hex}"
    created = int(time.time())
    tool_mode = False

    try:
        async for chunk in response_gen:
            buffer += chunk

            if not tool_mode:
                if TOOL_MARKER in buffer:
                    tool_mode = True
                    idx = buffer.index(TOOL_MARKER)
                    text_before = buffer[:idx]
                    buffer = buffer[idx:]
                    if text_before.strip():
                        yield format_sse({
                            "id": chunk_id, "object": "chat.completion.chunk",
                            "created": created, "model": model,
                            "choices": [{"index": 0, "delta": {"content": text_before}, "finish_reason": None}],
                        })

            if tool_mode:
                if _has_complete_tool_call(buffer):
                    calls = extract_tool_calls(buffer)
                    async for _ in response_gen:
                        pass
                    indexed_calls = [
                        {"index": i, **tc} for i, tc in enumerate(calls)
                    ]
                    yield format_sse({
                        "id": chunk_id, "object": "chat.completion.chunk",
                        "created": created, "model": model,
                        "choices": [{
                            "index": 0,
                            "delta": {"role": "assistant", "content": None, "tool_calls": indexed_calls},
                            "finish_reason": "tool_calls",
                        }],
                    })
                    yield "data: [DONE]\n\n"
                    return
                if len(buffer) > TOOL_BUFFER_LIMIT:
                    log.warning("Tool buffer exceeded hard limit, flushing as text")
                    tool_mode = False
                    yield format_sse({
                        "id": chunk_id, "object": "chat.completion.chunk",
                        "created": created, "model": model,
                        "choices": [{"index": 0, "delta": {"content": buffer}, "finish_reason": None}],
                    })
                    buffer = ""
                continue

            if len(buffer) > TEXT_FLUSH_THRESHOLD:
                if _has_complete_tool_call(buffer):
                    tool_mode = True
                    continue
                yield format_sse({
                    "id": chunk_id, "object": "chat.completion.chunk",
                    "created": created, "model": model,
                    "choices": [{"index": 0, "delta": {"content": buffer}, "finish_reason": None}],
                })
                buffer = ""
    except Exception as e:
        log.error("Stream error [%s]: %s", type(e).__name__, repr(e))
        yield format_sse({
            "id": chunk_id, "object": "chat.completion.chunk",
            "created": created, "model": model,
            "choices": [{
                "index": 0,
                "delta": {"content": buffer} if buffer else {},
                "finish_reason": "stop",
            }],
        })
        yield "data: [DONE]\n\n"
        return

    if buffer:
        if _has_complete_tool_call(buffer):
            calls = extract_tool_calls(buffer)
            indexed_calls = [
                {"index": i, **tc} for i, tc in enumerate(calls)
            ]
            yield format_sse({
                "id": chunk_id, "object": "chat.completion.chunk",
                "created": created, "model": model,
                "choices": [{
                    "index": 0,
                    "delta": {"role": "assistant", "content": None, "tool_calls": indexed_calls},
                    "finish_reason": "tool_calls",
                }],
            })
            yield "data: [DONE]\n\n"
            return

        full_text = buffer
        completion_tokens = estimate_tokens(full_text)
        yield format_sse({
            "id": chunk_id, "object": "chat.completion.chunk",
            "created": created, "model": model,
            "choices": [{"index": 0, "delta": {"content": full_text}, "finish_reason": "stop"}],
            "usage": {
                "prompt_tokens": prompt_tokens,
                "completion_tokens": completion_tokens,
                "total_tokens": prompt_tokens + completion_tokens,
            },
        })

    yield "data: [DONE]\n\n"
