"""A logical chat turn owns its conversation through generation and repair."""

import asyncio
import logging
from contextlib import aclosing

from jsonschema import Draft202012Validator, SchemaError

from .errors import ProxyError
from .images import load_image
from .prompts import build_prompt, get_new_messages, get_thread_id
from .schemas import NamedToolChoice
from .settings import estimate_tokens
from .tools.parser import Event, ToolParser
from .tools.recovery import Recovery

log = logging.getLogger(__name__)


class ChatService:
    def __init__(self, pool, settings):
        self.pool = pool
        self.settings = settings

    def validate(self, request):
        # Hermes usually sends the public OpenAI model id (for example
        # ``deepseek-chat``), while the backend uses its internal model names.
        # Keep explicit expert/vision selection, and treat other common chat
        # ids as the default model.
        supported = {"DEFAULT", "EXPERT", "VISION", "DEEPSEEK-CHAT", "DEEPSEEK-V3"}
        if request.model.upper() not in supported:
            raise ProxyError("Unknown model; see /v1/models", 400, "model_not_found", "model")
        for tool in request.tools or []:
            try:
                Draft202012Validator.check_schema(tool.function.parameters)

                # Remote schema retrieval must never happen during validation.
                def check_refs(node):
                    if isinstance(node, dict):
                        for key, value in node.items():
                            if key in ("$ref", "$dynamicRef") and not str(value).startswith("#"):
                                raise ValueError("External schema references are unsupported")
                            check_refs(value)
                    elif isinstance(node, list):
                        for value in node:
                            check_refs(value)

                check_refs(tool.function.parameters)
            except (SchemaError, ValueError) as exc:
                raise ProxyError(
                    "Invalid or unsupported tool schema", 400, "invalid_tool_schema", "tools"
                ) from exc

    async def events(self, request):
        thread_id = get_thread_id(request)
        tools = request.tools or []
        if request.tool_choice == "none":
            tools = []
        elif isinstance(request.tool_choice, NamedToolChoice):
            tools = [t for t in tools if t.function.name == request.tool_choice.function.name]
        recovery = Recovery(
            tools,
            request.tool_choice == "required" or isinstance(request.tool_choice, NamedToolChoice),
            request.parallel_tool_calls,
        )
        try:
            async with asyncio.timeout(self.settings.request_timeout):
                async with self.pool.turn(thread_id) as turn:
                    messages = get_new_messages(request.messages)
                    prev = self.pool.get_thread_exchanges(thread_id)
                    count = sum(m.role in ("user", "tool") for m in messages)
                    interval = self.settings.system_prompt_interval
                    reinforce = interval > 0 and (prev + count) // interval > prev // interval
                    first = not any(m.role == "assistant" for m in request.messages)
                    prompt, _ = build_prompt(
                        messages,
                        tools,
                        first or reinforce,
                        first or reinforce,
                        prev,
                        request.messages,
                    )
                    if tools:
                        prompt += "\nOnly call currently available tools: " + ", ".join(
                            t.function.name for t in tools
                        )
                        if recovery.required:
                            prompt += "\nYou must return a tool call."
                        if not request.parallel_tool_calls:
                            prompt += "\nReturn at most one tool call."
                    elif request.tool_choice == "none":
                        prompt += "\nDo not call tools in this turn; reply with normal text."
                    image = await load_image(messages, self.settings.image_max_bytes)
                    prompt_tokens = self.pool.get_thread_tokens(thread_id) + estimate_tokens(prompt)
                    context_added = estimate_tokens(prompt)
                    raw_response = []
                    parser = ToolParser(
                        enabled=bool(tools), buffer_limit=self.settings.tool_buffer_limit
                    )
                    async with aclosing(
                        turn.generate(
                            prompt, model=request.model, stream=request.stream, image=image
                        )
                    ) as source:
                        async for text in source:
                            raw_response.append(text)
                            for event in parser.feed(text):
                                accepted = recovery.accept(event)
                                if accepted:
                                    yield accepted
                    for event in parser.finish():
                        accepted = recovery.accept(event)
                        if accepted:
                            yield accepted
                    context_added += estimate_tokens("".join(raw_response))
                    recovery.require_call()
                    for attempt in range(1, self.settings.tool_repair_max_retries + 1):
                        if not recovery.pending:
                            break
                        repair_prompt = recovery.begin_repair()
                        log.warning(
                            "Tool repair attempt=%d unresolved=%d", attempt, recovery.expected
                        )
                        repair_parser = ToolParser(
                            enabled=True, buffer_limit=self.settings.tool_buffer_limit
                        )
                        repair_raw = []
                        async with aclosing(
                            turn.generate(repair_prompt, model=request.model, stream=False)
                        ) as source:
                            async for text in source:
                                repair_raw.append(text)
                                for event in repair_parser.feed(text):
                                    if event.kind != "text":
                                        accepted = recovery.accept(event)
                                        if accepted:
                                            yield accepted
                        for event in repair_parser.finish():
                            if event.kind != "text":
                                accepted = recovery.accept(event)
                                if accepted:
                                    yield accepted
                        context_added += estimate_tokens(repair_prompt) + estimate_tokens(
                            "".join(repair_raw)
                        )
                        recovery.end_repair()
                        log.info(
                            "Tool repair attempt=%d remaining=%d", attempt, len(recovery.pending)
                        )
                    # Persist the context consumed, including internal corrections.
                    self.pool.add_thread_tokens(thread_id, context_added)
                    self.pool.bump_thread_exchanges(thread_id, count)
                    if recovery.pending:
                        raise ProxyError(
                            "Could not recover a valid tool response", code="tool_repair_failed"
                        )
                    yield Event(
                        "done",
                        {
                            "prompt_tokens": prompt_tokens,
                            "finish_reason": "tool_calls" if recovery.calls else "stop",
                        },
                    )
        except TimeoutError as exc:
            raise ProxyError("Request processing timed out", 504, "request_timeout") from exc
