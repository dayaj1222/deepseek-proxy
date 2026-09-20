"""Prompt translation; backend I/O lives in the service."""

import hashlib
from typing import Any, List, Optional
from .schemas import ChatRequest, Message, Tool
from .settings import settings, render_prompt, TOOL_REMINDER_INTERVAL
from .tools.format import (
    inject_tool_descriptions,
    format_tool_calls_for_history,
    format_reminder_text,
)
from .observability import get_logger

log = get_logger(__name__)


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
        if msg.role in ("system", "developer"):
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
            return (
                prefix
                + hashlib.sha256(
                    ((request.user + "\0" if request.user else "") + content).encode()
                ).hexdigest()[:24]
            )
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
    system_messages: Optional[List[Message]] = None,
) -> tuple[str, Optional[bytes]]:
    """Build a plain-text prompt from the message list.

    Returns (prompt, image_bytes|None). Only the FIRST image found in
    new user messages is forwarded — DeepSeek web takes one image per
    turn (ref_file_ids single-element). Extra images degrade to [image].
    Role prefixes, the continuation line, and the periodic format reminder all
    come from config.toml [prompts], rendered through render_prompt (which
    resolves the tool-call tag placeholders). On periodic system re-anchoring,
    system_messages supplies the original system message because messages is
    normally only the delta after the last assistant response.
    """
    prompts = settings.prompts
    parts = []
    system_content = None
    other_messages = []

    # Delta prompts normally contain no system message after the first turn.
    # Keep the full request available so a periodic re-anchor can restore it.
    source_messages = system_messages or messages
    for msg in source_messages:
        if msg.role in ("system", "developer"):
            system_content = "\n\n".join(
                filter(None, [system_content, normalize_content(msg.content)])
            )
    for msg in messages:
        if msg.role not in ("system", "developer"):
            other_messages.append(msg)

    if include_system:
        system_text = system_content or ""
        if include_tools and tools:
            system_text = inject_tool_descriptions(system_text, [t.model_dump() for t in tools])
        if system_text:
            prefix = prompts.get("ROLE_PREFIX_SYSTEM", "System: {content}")
            parts.append(render_prompt(prefix, content=system_text))

    last_role = None
    image_bytes: Optional[bytes] = None
    for msg in other_messages:
        if msg.role == "user":
            text, imgs = normalize_content(msg.content), []
            if imgs and image_bytes is None:
                image_bytes = imgs[0]
                if len(imgs) > 1:
                    log.warning(
                        "Multiple images in one turn - forwarding first, dropping %d", len(imgs) - 1
                    )
                if not text.strip():
                    text = "[image]"
                content = text
            else:
                content = normalize_content(msg.content)
            prefix = prompts.get("ROLE_PREFIX_USER", "User: {content}")
            parts.append(render_prompt(prefix, content=content))
        elif msg.role == "assistant":
            content = normalize_content(msg.content)
            if msg.tool_calls:
                if content:
                    prefix = prompts.get("ROLE_PREFIX_ASSISTANT", "Assistant: {content}")
                    parts.append(render_prompt(prefix, content=content))
                prefix = prompts.get("ROLE_PREFIX_ASSISTANT_TOOL", "Assistant:\n{content}")
                parts.append(
                    render_prompt(prefix, content=format_tool_calls_for_history(msg.tool_calls))
                )
            else:
                prefix = prompts.get("ROLE_PREFIX_ASSISTANT", "Assistant: {content}")
                parts.append(render_prompt(prefix, content=content))
        elif msg.role == "tool":
            content = normalize_content(msg.content)
            prefix = prompts.get("ROLE_PREFIX_TOOL", "Tool result (id={tool_call_id}):\n{content}")
            parts.append(render_prompt(prefix, tool_call_id=msg.tool_call_id, content=content))
        last_role = msg.role

    if last_role == "tool":
        parts.append(
            prompts.get(
                "CONTINUE_AFTER_TOOL",
                "Now continue with the task based on the tool result above.",
            )
        )

    interval = TOOL_REMINDER_INTERVAL
    count = sum(m.role in ("user", "tool") for m in messages)
    if (
        tools
        and interval > 0
        and (exchange_offset + count) // interval > exchange_offset // interval
    ):
        parts.append(format_reminder_text())
    return "\n\n".join(parts), image_bytes
