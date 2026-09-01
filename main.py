import asyncio
import atexit
import hashlib
import json
import logging
import os
import signal
import time
import uuid
from contextlib import asynccontextmanager
from typing import Any, Dict, List, Optional, Union

from fastapi import FastAPI, Request
from fastapi.responses import JSONResponse, StreamingResponse
from pydantic import BaseModel

from config import (
    MODELS,
    PROXY_HOST,
    PROXY_PORT,
    TOOL_CALL_TEMPLATE,
    TOOL_REMINDER_INTERVAL,
    estimate_tokens,
    render_prompt,
    settings,
)
from deepseek_client import (
    add_thread_tokens,
    bump_thread_exchanges,
    flush_state,
    generate_response,
    get_thread_exchanges,
    get_thread_tokens,
    init_state,
    shutdown_client,
)
from streaming_handler import hybrid_stream_generator
from tool_parser import (
    extract_tool_calls,
    format_tool_calls_for_history,
    inject_tool_descriptions,
)

from logger import clear_request_id, get_logger, set_request_id

log = get_logger(__name__)


# ---- Pydantic models ----
def normalize_content(content: Any) -> str:
    """Convert OpenAI content (string or list of parts) to plain text."""
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        parts = []
        for part in content:
            if isinstance(part, dict):
                if part.get("type") == "text":
                    parts.append(part.get("text", ""))
                elif part.get("type") == "image_url":
                    parts.append("[image]")
                else:
                    parts.append(str(part))
            else:
                parts.append(str(part))
        return "\n".join(parts)
    return str(content) if content else ""


class Message(BaseModel):
    role: str
    content: Optional[Union[str, List[Dict[str, Any]]]] = None
    tool_calls: Optional[List[Any]] = None
    tool_call_id: Optional[str] = None
    name: Optional[str] = None


class ToolFunction(BaseModel):
    name: str
    description: Optional[str] = None
    parameters: Optional[Dict[str, Any]] = None


class Tool(BaseModel):
    type: str = "function"
    function: ToolFunction


class ChatRequest(BaseModel):
    model: str
    messages: List[Message]
    tools: Optional[List[Tool]] = None
    tool_choice: Optional[Any] = "auto"
    stream: Optional[bool] = False
    temperature: Optional[float] = 0.7
    max_tokens: Optional[int] = None
    thread_id: Optional[str] = None


# ---- Helper functions ----
_TITLE_PROMPT_MARKER = "You name chat sessions"


def is_title_request(request: ChatRequest) -> bool:
    """Detect Hermes' session-title generation requests.

    Hermes fires title-gen auxiliary calls whose system prompt starts
    with "You name chat sessions" and whose first user message is the
    same as the real chat request for that turn. If both map to the
    same thread_id they share one DeepSeek Conversation, and the two
    concurrent asks can cross responses — the chat then receives the
    title JSON (observed in production). Routing title requests to
    their own thread namespace makes the collision impossible.
    """
    for msg in request.messages:
        if msg.role == "system":
            content = normalize_content(msg.content)
            if content.startswith(_TITLE_PROMPT_MARKER):
                return True
    return False


def get_thread_id(request: ChatRequest) -> str:
    """Derive a stable thread ID from the request, or use the provided one."""
    if request.thread_id:
        return request.thread_id
    prefix = "title_" if is_title_request(request) else "thread_"
    for msg in request.messages:
        content = normalize_content(msg.content)
        if msg.role == "user" and content:
            return prefix + hashlib.sha256(content.encode()).hexdigest()[:24]
    all_content = "".join(normalize_content(m.content) for m in request.messages)
    return prefix + hashlib.sha256(all_content.encode()).hexdigest()[:24]


def get_new_messages(messages: List[Message]) -> List[Message]:
    """
    Return only messages that arrived after the last assistant message.
    DeepSeek already holds the entire conversation history, so we only
    need to send the new user/tool messages.
    """
    last_assistant_idx = -1
    for i, msg in enumerate(messages):
        if msg.role == "assistant":
            last_assistant_idx = i
    return messages[last_assistant_idx + 1 :]


def build_prompt(
    messages: List[Message],
    tools: Optional[List[Tool]] = None,
    include_system: bool = True,
    include_tools: bool = True,
    exchange_offset: int = 0,
) -> str:
    """Build a plain-text prompt from the message list.

    Role prefixes, the continuation line, and the periodic format reminder all
    come from config.toml [prompts], rendered through render_prompt (which
    resolves the tool-call tag placeholders).
    """
    prompts = settings.prompts
    parts = []
    system_content = None
    other_messages = []

    for msg in messages:
        if msg.role == "system":
            system_content = normalize_content(msg.content)
        else:
            other_messages.append(msg)

    if include_system:
        system_text = system_content or ""
        if include_tools and tools:
            system_text = inject_tool_descriptions(
                system_text, [t.model_dump() for t in tools]
            )
        if system_text:
            prefix = prompts.get("ROLE_PREFIX_SYSTEM", "System: {content}")
            parts.append(render_prompt(prefix, content=system_text))

    last_role = None
    for msg in other_messages:
        content = normalize_content(msg.content)
        if msg.role == "user":
            prefix = prompts.get("ROLE_PREFIX_USER", "User: {content}")
            parts.append(render_prompt(prefix, content=content))
        elif msg.role == "assistant":
            if msg.tool_calls:
                if content:
                    prefix = prompts.get("ROLE_PREFIX_ASSISTANT", "Assistant: {content}")
                    parts.append(render_prompt(prefix, content=content))
                prefix = prompts.get(
                    "ROLE_PREFIX_ASSISTANT_TOOL", "Assistant:\n{content}"
                )
                parts.append(
                    render_prompt(
                        prefix, content=format_tool_calls_for_history(msg.tool_calls)
                    )
                )
            else:
                prefix = prompts.get("ROLE_PREFIX_ASSISTANT", "Assistant: {content}")
                parts.append(render_prompt(prefix, content=content))
        elif msg.role == "tool":
            prefix = prompts.get(
                "ROLE_PREFIX_TOOL", "Tool result (id={tool_call_id}):\n{content}"
            )
            parts.append(
                render_prompt(
                    prefix, tool_call_id=msg.tool_call_id, content=content
                )
            )
        last_role = msg.role

    if last_role == "tool":
        parts.append(
            prompts.get(
                "CONTINUE_AFTER_TOOL",
                "Now continue with the task based on the tool result above.",
            )
        )

    # Re-anchor the tool-call format every N user/tool messages (N from
    # TOOL_REMINDER_INTERVAL). Prompts are deltas (only new messages), so
    # exchange_offset carries the per-thread cumulative count across requests.
    if tools and TOOL_REMINDER_INTERVAL > 0:
        hard_reminder = render_prompt(prompts.get("FORMAT_REMINDER", ""))
        exchange_count = exchange_offset
        final_parts = []
        for part in parts:
            final_parts.append(part)
            if part.startswith("User: ") or part.startswith("Tool result (id="):
                exchange_count += 1
                if exchange_count % TOOL_REMINDER_INTERVAL == 0:
                    final_parts.append(hard_reminder)
        parts = final_parts
    prompt = "\n\n".join(parts)
    return prompt



# ---- Main request handler ----
async def handle_chat_request(request: ChatRequest):
    thread_id = get_thread_id(request)
    set_request_id(thread_id)
    try:
        return await _handle(request, thread_id)
    finally:
        clear_request_id()


async def _handle(request: ChatRequest, thread_id: str):
    is_first = not any(m.role == "assistant" for m in request.messages)
    new_messages = get_new_messages(request.messages)

    if settings.debug:
        log.debug("Request body: %s", request.model_dump_json(indent=2))

    log.info(
        "chat/completions  thread=%s  total=%d  new=%d  tools=%d  first=%s",
        thread_id,
        len(request.messages),
        len(new_messages),
        len(request.tools) if request.tools else 0,
        is_first,
    )

    # Count this turn's exchanges (user/tool messages) for reminder pacing
    prev_exchanges = get_thread_exchanges(thread_id)
    turn_exchanges = sum(1 for m in new_messages if m.role in ("user", "tool"))

    prompt = build_prompt(
        new_messages,
        request.tools,
        include_system=is_first,
        include_tools=is_first,
        exchange_offset=prev_exchanges,
    )
    bump_thread_exchanges(thread_id, turn_exchanges)
    # Cumulative accounting: report the full context the model sees this turn
    # (DeepSeek holds prior turns server-side), then grow the stored total.
    base_tokens = get_thread_tokens(thread_id)
    turn_prompt_tokens = estimate_tokens(prompt)
    prompt_tokens = base_tokens + turn_prompt_tokens

    if settings.debug:
        log.debug("Prompt (%d chars):\n%s", len(prompt), prompt)

    if request.stream:
        response_gen = generate_response(
            thread_id, prompt, model=request.model, stream=True
        )
        return StreamingResponse(
            hybrid_stream_generator(
                response_gen,
                request.model,
                thread_id,
                prompt_tokens,
                turn_prompt_tokens,
            ),
            media_type="text/event-stream",
        )

    full_response = ""
    async for chunk in generate_response(
        thread_id, prompt, model=request.model, stream=False
    ):
        full_response = chunk

    log.info("Response received, %d chars", len(full_response))

    tool_calls = extract_tool_calls(full_response) if request.tools else []
    log.info("Tool calls detected: %d", len(tool_calls))
    if settings.debug and tool_calls:
        log.debug("Extracted tool calls: %s", json.dumps(tool_calls, indent=2))
    for tc in tool_calls:
        log.info(
            "  tool: %s  args=%s", tc["function"]["name"], tc["function"]["arguments"]
        )

    completion_tokens = estimate_tokens(full_response or json.dumps(tool_calls))
    add_thread_tokens(thread_id, turn_prompt_tokens + completion_tokens)

    if tool_calls:
        chunk_id = f"chatcmpl-{uuid.uuid4().hex}"
        return JSONResponse(
            content={
                "id": chunk_id,
                "object": "chat.completion",
                "created": int(time.time()),
                "model": request.model,
                "choices": [
                    {
                        "index": 0,
                        "message": {
                            "role": "assistant",
                            "content": None,
                            "tool_calls": tool_calls,
                        },
                        "finish_reason": "tool_calls",
                    }
                ],
                "usage": {
                    "prompt_tokens": prompt_tokens,
                    "completion_tokens": completion_tokens,
                    "total_tokens": prompt_tokens + completion_tokens,
                },
            }
        )

    return JSONResponse(
        content={
            "id": f"chatcmpl-{uuid.uuid4().hex}",
            "object": "chat.completion",
            "created": int(time.time()),
            "model": request.model,
            "choices": [
                {
                    "index": 0,
                    "message": {"role": "assistant", "content": full_response},
                    "finish_reason": "stop",
                }
            ],
            "usage": {
                "prompt_tokens": prompt_tokens,
                "completion_tokens": completion_tokens,
                "total_tokens": prompt_tokens + completion_tokens,
            },
        }
    )


# ---- App lifecycle ----
@asynccontextmanager
async def lifespan(app: FastAPI):
    log.info("Proxy starting up")
    init_state()

    def _flush_and_die(signum, _frame):
        log.info("Signal %s received — flushing state to disk", signum)
        flush_state()
        signal.signal(signum, signal.SIG_DFL)
        os.kill(os.getpid(), signum)

    # uvicorn handles SIGINT/SIGTERM gracefully (runs the shutdown below);
    # these would otherwise kill the process with no flush:
    for sig in (signal.SIGHUP, signal.SIGQUIT):
        try:
            signal.signal(sig, _flush_and_die)
        except (ValueError, OSError, AttributeError):
            pass

    atexit.register(flush_state)
    try:
        yield
    finally:
        log.info("Proxy shutting down")
        flush_state()
        await shutdown_client()


app = FastAPI(title="DeepSeek OpenAI Proxy", lifespan=lifespan)


@app.middleware("http")
async def log_requests(request: Request, call_next):
    body = await request.body()
    # Store the body so downstream consumers can read it again
    request._body = body
    return await call_next(request)


@app.post("/v1/chat/completions")
async def chat_completions(request: Request):
    raw = await request.body()
    try:
        parsed = json.loads(raw)
        # Validate manually for debugging
        try:
            chat_req = ChatRequest(**parsed)
        except Exception as ve:
            log.error(
                "Validation error for request:\n%s",
                json.dumps(parsed, indent=2)[:10000],
            )
            log.error("Validation error details: %s", ve)
            return JSONResponse(status_code=422, content={"detail": str(ve)})
        return await handle_chat_request(chat_req)
    except json.JSONDecodeError:
        return JSONResponse(status_code=400, content={"detail": "Invalid JSON"})


@app.get("/v1/models")
async def list_models():
    return {
        "object": "list",
        "data": MODELS,
    }


if __name__ == "__main__":
    import uvicorn

    uvicorn.run(app, host=PROXY_HOST, port=PROXY_PORT)
