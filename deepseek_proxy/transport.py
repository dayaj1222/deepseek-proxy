"""OpenAI response encoding, bounded stream buffering, and heartbeats."""

import asyncio
import json
import logging
from contextlib import aclosing, suppress
from time import time
from uuid import uuid4

from .errors import ProxyError
from .settings import estimate_tokens

log = logging.getLogger(__name__)


def error_for(exc):
    from .backend import BackendRateLimited, AccountQueueFull, AccountQueueTimeout

    if isinstance(exc, ProxyError):
        return exc
    if isinstance(exc, (AccountQueueFull, AccountQueueTimeout)):
        return ProxyError("Request queue unavailable; try again later", 503, "queue_unavailable")
    if isinstance(exc, BackendRateLimited):
        error = ProxyError("Backend capacity temporarily unavailable", 429, "rate_limit_exceeded")
        error.retry_after = exc.retry_after
        return error
    if type(exc).__name__ in (
        "QueueFull",
        "QueueTimeout",
        "AccountQueueFull",
        "AccountQueueTimeout",
    ):
        return ProxyError("Request queue unavailable; try again later", 503, "queue_unavailable")
    log.exception("Backend request failed", exc_info=exc)
    return ProxyError("Backend request failed")


def sse(data):
    return "data: " + json.dumps(data, ensure_ascii=False) + "\n\n"


class Completion:
    def __init__(self, model):
        self.id = "chatcmpl-" + uuid4().hex
        self.created = int(time())
        self.model = model
        self.text = []
        self.calls = []
        self.done = None

    def base(self, stream=False):
        return {
            "id": self.id,
            "created": self.created,
            "model": self.model,
            "object": "chat.completion.chunk" if stream else "chat.completion",
        }

    def chunk(self, delta, finish=None):
        return {
            **self.base(True),
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
        }

    def accept(self, event):
        if event.kind == "text":
            self.text.append(event.value)
            return self.chunk({"content": event.value})
        if event.kind == "tool":
            index = len(self.calls)
            self.calls.append(event.value)
            return self.chunk({"tool_calls": [{"index": index, **event.value}]})
        if event.kind == "done":
            self.done = event.value
        return None

    def usage(self):
        completion = estimate_tokens("".join(self.text))
        completion += sum(
            estimate_tokens(c["function"]["name"] + c["function"]["arguments"]) for c in self.calls
        )
        prompt = self.done["prompt_tokens"]
        return {
            "prompt_tokens": prompt,
            "completion_tokens": completion,
            "total_tokens": prompt + completion,
        }

    def response(self):
        message = {
            "role": "assistant",
            "content": "".join(self.text) or (None if self.calls else ""),
        }
        if self.calls:
            message["tool_calls"] = self.calls
        return {
            **self.base(),
            "choices": [
                {"index": 0, "message": message, "finish_reason": self.done["finish_reason"]}
            ],
            "usage": self.usage(),
        }


async def collect(service, request):
    completion = Completion(request.model)
    async with aclosing(service.events(request)) as events:
        async for event in events:
            completion.accept(event)
    return completion.response()


async def stream(service, request):
    queue = asyncio.Queue(maxsize=32)
    completion = Completion(request.model)
    sentinel = object()

    async def produce():
        try:
            async with aclosing(service.events(request)) as events:
                async for event in events:
                    await queue.put(event)
        except Exception as exc:
            await queue.put(error_for(exc))
        finally:
            # Cancellation must never block trying to write to a full queue.
            if not asyncio.current_task().cancelling():
                await queue.put(sentinel)

    producer = asyncio.create_task(produce())
    try:
        yield sse(completion.chunk({"role": "assistant", "content": ""}))
        while True:
            try:
                event = await asyncio.wait_for(queue.get(), service.settings.heartbeat_interval)
            except TimeoutError:
                yield ": keep-alive\n\n"
                continue
            if event is sentinel:
                break
            if isinstance(event, ProxyError):
                yield sse(event.payload())
                yield "data: [DONE]\n\n"
                return
            chunk = completion.accept(event)
            if chunk:
                if request.stream_options and request.stream_options.include_usage:
                    chunk["usage"] = None
                yield sse(chunk)
        yield sse(completion.chunk({}, completion.done["finish_reason"]))
        if request.stream_options and request.stream_options.include_usage:
            yield sse({**completion.base(True), "choices": [], "usage": completion.usage()})
        yield "data: [DONE]\n\n"
    finally:
        producer.cancel()
        with suppress(asyncio.CancelledError):
            await producer
