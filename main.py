import asyncio
import hashlib
import json
import logging
import time
import uuid
from contextlib import asynccontextmanager
from typing import Any, Dict, List, Optional, Union

from fastapi import FastAPI, Request
from fastapi.responses import JSONResponse, StreamingResponse
from pydantic import BaseModel

from config import PROXY_HOST, PROXY_PORT, REQUEST_DELAY, estimate_tokens
from deepseek_client import generate_response, shutdown_client
from streaming_handler import hybrid_stream_generator
from tool_parser import extract_tool_calls, inject_tool_descriptions


logging.basicConfig(
    level=logging.DEBUG, format="%(asctime)s [%(levelname)s] %(message)s"
)
log = logging.getLogger(__name__)


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
def get_thread_id(request: ChatRequest) -> str:
    """Derive a stable thread ID from the request, or use the provided one."""
    if request.thread_id:
        return request.thread_id
    for msg in request.messages:
        content = normalize_content(msg.content)
        if msg.role == "user" and content:
            return "thread_" + hashlib.sha256(content.encode()).hexdigest()[:24]
    all_content = "".join(normalize_content(m.content) for m in request.messages)
    return "thread_" + hashlib.sha256(all_content.encode()).hexdigest()[:24]


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
) -> str:
    """Build a plain‑text prompt from the message list."""
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
            parts.append(f"System: {system_text}")

    last_role = None
    for msg in other_messages:
        content = normalize_content(msg.content)
        if msg.role == "user":
            parts.append(f"User: {content}")
        elif msg.role == "assistant":
            if msg.tool_calls:
                parts.append(
                    f"Assistant (tool calls):\n{json.dumps(msg.tool_calls, indent=2)}"
                )
            else:
                parts.append(f"Assistant: {content}")
        elif msg.role == "tool":
            parts.append(f"Tool result (id={msg.tool_call_id}):\n{content}")
        last_role = msg.role

    if last_role == "tool":
        parts.append("Now continue with the task based on the tool result above.")

    prompt = "\n\n".join(parts)
    log.debug("Built prompt (%d chars, include_system=%s)", len(prompt), include_system)
    return prompt


# ---- Main request handler ----
async def handle_chat_request(request: ChatRequest):
    thread_id = get_thread_id(request)
    is_first = not any(m.role == "assistant" for m in request.messages)
    new_messages = get_new_messages(request.messages)

    log.info(
        "chat/completions  thread=%s  total=%d  new=%d  tools=%d  first=%s",
        thread_id,
        len(request.messages),
        len(new_messages),
        len(request.tools) if request.tools else 0,
        is_first,
    )

    prompt = build_prompt(
        new_messages, request.tools, include_system=is_first, include_tools=is_first
    )
    prompt_tokens = estimate_tokens(str(request.messages))

    if REQUEST_DELAY > 0:
        log.info("Delaying request by %.2f seconds", REQUEST_DELAY)
        await asyncio.sleep(REQUEST_DELAY)

    if request.stream:
        response_gen = generate_response(thread_id, prompt, model=request.model, stream=True)
        return StreamingResponse(
            hybrid_stream_generator(response_gen, request.model, thread_id, prompt_tokens),
            media_type="text/event-stream",
        )

    full_response = ""
    async for chunk in generate_response(thread_id, prompt, model=request.model, stream=False):
        full_response = chunk

    log.debug("DeepSeek response:\n%s", full_response)

    tool_calls = extract_tool_calls(full_response) if request.tools else []
    log.info("Tool calls detected: %d", len(tool_calls))
    for tc in tool_calls:
        log.info(
            "  tool: %s  args=%s", tc["function"]["name"], tc["function"]["arguments"]
        )

    completion_tokens = estimate_tokens(full_response or json.dumps(tool_calls))

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
    yield
    log.info("Proxy shutting down")
    await shutdown_client()


app = FastAPI(title="DeepSeek OpenAI Proxy", lifespan=lifespan)


@app.middleware("http")
async def log_requests(request: Request, call_next):
    body = await request.body()
    if body:
        try:
            parsed = json.loads(body)
            log.debug(
                "Incoming %s %s\n%s",
                request.method,
                request.url.path,
                json.dumps(parsed, indent=2)[:10000],
            )
        except Exception:
            pass
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
            log.error("Validation error for request:\n%s", json.dumps(parsed, indent=2)[:10000])
            log.error("Validation error details: %s", ve)
            return JSONResponse(status_code=422, content={"detail": str(ve)})
        return await handle_chat_request(chat_req)
    except json.JSONDecodeError:
        return JSONResponse(status_code=400, content={"detail": "Invalid JSON"})


@app.get("/v1/models")
async def list_models():
    return {
        "object": "list",
        "data": [
            {
                "id": "deepseek-v4-pro",
                "object": "model",
                "created": 1745452800,
                "owned_by": "deepseek",
                "context_length": 1000000,
                "max_completion_tokens": 384000,
            },
            {
                "id": "deepseek-v4-flash",
                "object": "model",
                "created": 1745452800,
                "owned_by": "deepseek",
                "context_length": 1000000,
                "max_completion_tokens": 384000,
            },
            {
                "id": "deepseek-chat",
                "object": "model",
                "created": 1677610602,
                "owned_by": "deepseek",
                "context_length": 1000000,
                "max_completion_tokens": 384000,
            },
            {
                "id": "deepseek-reasoner",
                "object": "model",
                "created": 1745452800,
                "owned_by": "deepseek",
                "context_length": 1000000,
                "max_completion_tokens": 384000,
            },
            {
                "id": "deepseek-v4",
                "object": "model",
                "created": 1745452800,
                "owned_by": "deepseek",
                "context_length": 1000000,
                "max_completion_tokens": 384000,
            },
        ],
    }


if __name__ == "__main__":
    import uvicorn

    uvicorn.run(app, host=PROXY_HOST, port=PROXY_PORT)
