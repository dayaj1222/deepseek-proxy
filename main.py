import time
import uuid
import json
import logging
from typing import List, Dict, Any, Optional, AsyncGenerator
from contextlib import asynccontextmanager

from fastapi import FastAPI, Request
from fastapi.responses import StreamingResponse, JSONResponse
from pydantic import BaseModel

from config import PROXY_HOST, PROXY_PORT
from deepseek_client import generate_response, shutdown_client
from tool_parser import extract_tool_calls, inject_tool_descriptions
from usage_tracker import update_usage

logging.basicConfig(level=logging.DEBUG, format="%(asctime)s [%(levelname)s] %(message)s")
log = logging.getLogger(__name__)

# ------------------------------------------------------------
# Models
# ------------------------------------------------------------
class Message(BaseModel):
    role: str
    content: Optional[str] = None
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

# ------------------------------------------------------------
# Thread ID
# ------------------------------------------------------------
def get_thread_id(request: ChatRequest) -> str:
    if request.thread_id:
        return request.thread_id
    import hashlib
    for msg in request.messages:
        if msg.role == "user" and msg.content:
            return "thread_" + hashlib.sha256(msg.content.encode()).hexdigest()[:24]
    all_content = "".join(m.content or "" for m in request.messages)
    return "thread_" + hashlib.sha256(all_content.encode()).hexdigest()[:24]

# ------------------------------------------------------------
# New messages only — no state tracking needed
# Find everything after the last assistant message.
# DeepSeek already has everything up to and including that.
# ------------------------------------------------------------
def get_new_messages(messages: List[Message]) -> List[Message]:
    last_assistant_idx = -1
    for i, msg in enumerate(messages):
        if msg.role == "assistant":
            last_assistant_idx = i
    return messages[last_assistant_idx + 1:]

# ------------------------------------------------------------
# Prompt builder
# ------------------------------------------------------------
def build_prompt(messages: List[Message], tools: Optional[List[Tool]] = None, include_system: bool = True) -> str:
    parts = []
    system_content = None
    other_messages = []

    for msg in messages:
        if msg.role == "system":
            system_content = msg.content or ""
        else:
            other_messages.append(msg)

    if include_system:
        if tools:
            system_content = inject_tool_descriptions(system_content or "", [t.model_dump() for t in tools])
        if system_content:
            parts.append(f"System: {system_content}")

    for msg in other_messages:
        if msg.role == "user":
            parts.append(f"User: {msg.content or ''}")
        elif msg.role == "assistant":
            if msg.tool_calls:
                parts.append(f"Assistant (tool calls):\n{json.dumps(msg.tool_calls, indent=2)}")
            else:
                parts.append(f"Assistant: {msg.content or ''}")
        elif msg.role == "tool":
            parts.append(f"Tool result (id={msg.tool_call_id}):\n{msg.content or ''}")

    prompt = "\n\n".join(parts)
    log.debug("Built prompt (%d chars, include_system=%s)", len(prompt), include_system)
    return prompt

# ------------------------------------------------------------
# SSE helpers
# ------------------------------------------------------------
def format_sse(data: Dict[str, Any]) -> str:
    return f"data: {json.dumps(data, ensure_ascii=False)}\n\n"

async def stream_generator(text: str, model: str, usage: dict) -> AsyncGenerator[str, None]:
    chunk_id = f"chatcmpl-{uuid.uuid4().hex}"
    for word in text.split(" "):
        yield format_sse({
            "id": chunk_id,
            "object": "chat.completion.chunk",
            "created": int(time.time()),
            "model": model,
            "choices": [{"index": 0, "delta": {"content": word + " "}, "finish_reason": None}],
        })
    yield format_sse({
        "id": chunk_id,
        "object": "chat.completion.chunk",
        "created": int(time.time()),
        "model": model,
        "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
        "usage": usage,
    })
    yield "data: [DONE]\n\n"

# ------------------------------------------------------------
# Main chat handler
# ------------------------------------------------------------
async def handle_chat_request(request: ChatRequest):
    thread_id = get_thread_id(request)
    is_first = not any(m.role == "assistant" for m in request.messages)
    new_messages = get_new_messages(request.messages)

    log.info(
        "chat/completions  thread=%s  total=%d  new=%d  tools=%d  first=%s",
        thread_id, len(request.messages), len(new_messages),
        len(request.tools) if request.tools else 0, is_first,
    )

    prompt = build_prompt(new_messages, request.tools if is_first else None, include_system=is_first)

    full_response = ""
    async for chunk in generate_response(thread_id, prompt, stream=False):
        full_response = chunk

    log.debug("DeepSeek response:\n%s", full_response)

    tool_calls = extract_tool_calls(full_response) if request.tools else []
    log.info("Tool calls detected: %d", len(tool_calls))

    if tool_calls:
        for tc in tool_calls:
            log.info("  tool: %s  args=%s", tc["function"]["name"], tc["function"]["arguments"])
        chunk_id = f"chatcmpl-{uuid.uuid4().hex}"
        usage = update_usage(thread_id, request.messages, full_response)
        if request.stream:
            async def tool_call_stream():
                yield format_sse({
                    "id": chunk_id,
                    "object": "chat.completion.chunk",
                    "created": int(time.time()),
                    "model": request.model,
                    "choices": [{"index": 0, "delta": {"role": "assistant", "content": None, "tool_calls": tool_calls}, "finish_reason": "tool_calls"}],
                })
                yield "data: [DONE]\n\n"
            return StreamingResponse(tool_call_stream(), media_type="text/event-stream")
        return JSONResponse(content={
            "id": chunk_id,
            "object": "chat.completion",
            "created": int(time.time()),
            "model": request.model,
            "choices": [{"index": 0, "message": {"role": "assistant", "content": None, "tool_calls": tool_calls}, "finish_reason": "tool_calls"}],
            "usage": {"prompt_tokens": usage["prompt_tokens"], "completion_tokens": usage["completion_tokens"], "total_tokens": usage["total_tokens"]},
        })

    usage = update_usage(thread_id, request.messages, full_response)
    if request.stream:
        return StreamingResponse(stream_generator(full_response, request.model, usage), media_type="text/event-stream")

    return JSONResponse(content={
        "id": f"chatcmpl-{uuid.uuid4().hex}",
        "object": "chat.completion",
        "created": int(time.time()),
        "model": request.model,
        "choices": [{"index": 0, "message": {"role": "assistant", "content": full_response}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": usage["prompt_tokens"], "completion_tokens": usage["completion_tokens"], "total_tokens": usage["total_tokens"]},
    })

# ------------------------------------------------------------
# Lifespan + App
# ------------------------------------------------------------
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
            log.debug("Incoming %s %s\n%s", request.method, request.url.path, json.dumps(parsed, indent=2)[:1000])
        except Exception:
            pass
    request._body = body
    return await call_next(request)

@app.post("/v1/chat/completions")
async def chat_completions(request: ChatRequest):
    return await handle_chat_request(request)

@app.get("/v1/models")
async def list_models():
    return {
        "object": "list",
        "data": [
            {
                "id": "deepseek-chat",
                "object": "model",
                "created": 1677610602,
                "owned_by": "deepseek",
                "context_length": 128000,
                "max_completion_tokens": 8192
            }
        ]
    }

if __name__ == "__main__":
    import uvicorn
    uvicorn.run(app, host=PROXY_HOST, port=PROXY_PORT)
