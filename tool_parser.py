"""
Tool‑call extraction and tool‑description injection for the DeepSeek OpenAI proxy.

The module parses raw model output to extract OpenAI‑compatible tool_call objects
and builds a system prompt that instructs the model how to format them.
"""

import json
import logging
import re
import uuid
from typing import Any, Dict, List, Optional

log = logging.getLogger(__name__)


def extract_tool_calls(text: str) -> List[Dict[str, Any]]:
    """
    Extract OpenAI‑style tool calls from plain‑text model output.

    Returns a list of tool_call dicts, each with ``id``, ``type``, and ``function`` keys.

    Parsing strips the ``⟿`` delimiter if present and attempts:
    1. Markdown fenced JSON blocks (`` ```json ... ``` ``)
    2. Bare JSON objects that contain a ``"function"`` key
    3. Balanced‑brace scanning (handles nested JSON correctly)
    """
    text = text.replace("⟿", "").strip()
    tool_calls: List[Dict[str, Any]] = []

    # 1. Markdown fenced JSON blocks
    fenced_blocks = re.findall(r'```(?:json)?\s*([\s\S]*?)\s*```', text)
    for block in fenced_blocks:
        parsed = _parse_block(block.strip())
        if parsed:
            tool_calls.extend(parsed)
            log.debug("Extracted %d tool call(s) from fenced block", len(parsed))
    if tool_calls:
        return tool_calls

    # 2. Bare JSON objects containing the word "function"
    bare_json = re.findall(r'\{[^`]*?"function"[^`]*?\}', text, re.DOTALL)
    for candidate in bare_json:
        parsed = _parse_block(candidate.strip())
        if parsed:
            tool_calls.extend(parsed)
    if tool_calls:
        log.debug("Extracted %d tool call(s) from bare JSON", len(tool_calls))
        return tool_calls

    # 3. Balanced‑brace scan (last resort, handles nested structures)
    depth = 0
    start = -1
    for i, ch in enumerate(text):
        if ch == '{':
            if depth == 0:
                start = i
            depth += 1
        elif ch == '}':
            depth -= 1
            if depth == 0 and start != -1:
                candidate = text[start:i + 1]
                parsed = _parse_block(candidate.strip())
                if parsed:
                    tool_calls.extend(parsed)
                start = -1
    if tool_calls:
        log.debug("Extracted %d tool call(s) via brace scan", len(tool_calls))

    return tool_calls


def _parse_block(block: str) -> List[Dict[str, Any]]:
    """
    Parse a JSON string and convert any tool‑call structures.

    Returns a list of tool_call dicts.  The input can be a single JSON object,
    a list of objects, or a combination.
    """
    try:
        data = json.loads(block)
    except json.JSONDecodeError:
        return []

    if isinstance(data, dict):
        tc = _to_tool_call(data)
        return [tc] if tc else []

    if isinstance(data, list):
        results = []
        for item in data:
            tc = _to_tool_call(item)
            if tc:
                results.append(tc)
        return results

    return []


def _to_tool_call(data: Dict[str, Any]) -> Optional[Dict[str, Any]]:
    """
    Convert a parsed dictionary to an OpenAI tool_call object.

    Supported shapes:
    * ``{"function": "<name>", "arguments": {...}}``   (most common raw output)
    * ``{"name": "<name>", "arguments": {...}}``       (alternative)
    * ``{"function": {"name": "...", "arguments": {...}}}`` (native OpenAI format)
    """
    func_name: Optional[str] = None
    arguments: Any = {}

    func_value = data.get("function")

    if isinstance(func_value, str):
        # {"function": "tool_name", "arguments": {...}}
        func_name = func_value
        arguments = data.get("arguments", {})
    elif isinstance(func_value, dict):
        # {"function": {"name": "...", "arguments": {...}}}
        func_name = func_value.get("name")
        arguments = func_value.get("arguments", {})
    elif "name" in data:
        # {"name": "tool_name", "arguments": {...}}
        func_name = data["name"]
        arguments = data.get("arguments", {})

    if not func_name:
        return None

    # Normalise arguments to a JSON string
    if isinstance(arguments, dict):
        args_str = json.dumps(arguments)
    elif isinstance(arguments, str):
        try:
            json.loads(arguments)      # Validate existing JSON
            args_str = arguments
        except json.JSONDecodeError:
            args_str = json.dumps({"input": arguments})
    else:
        args_str = json.dumps(arguments)

    call_id = f"call_{uuid.uuid4().hex[:16]}"
    log.debug("Parsed tool call: %s  id=%s", func_name, call_id)

    return {
        "id": call_id,
        "type": "function",
        "function": {
            "name": func_name,
            "arguments": args_str,
        },
    }


def inject_tool_descriptions(system_prompt: str, tools: List[Dict[str, Any]]) -> str:
    """
    Append tool definitions and usage instructions to the system prompt.

    The generated block matches the format expected by :func:`_to_tool_call`,
    so the model knows exactly what JSON shape to emit.
    """
    if not tools:
        return system_prompt

    tool_sections = []
    for tool in tools:
        if tool.get("type") != "function":
            continue
        func = tool["function"]
        params = json.dumps(func.get("parameters", {}), indent=2)
        tool_sections.append(
            f"### {func['name']}\n"
            f"Description: {func.get('description', 'No description provided.')}\n"
            f"Parameters (JSON Schema):\n```json\n{params}\n```"
        )

    tools_block = "\n\n".join(tool_sections)

    instruction = (
        "\n\n"
        "---\n"
        "## Proxy Wire Protocol (CRITICAL)\n\n"
        "Your response is processed by an OpenAI-compatible proxy. "
        "The following rules are REQUIRED for tool calls to work.\n\n"
        "### 1. Required Delimiter\n\n"
        "MUST wrap every tool call with the marker character ⟿ (U+27FF):\n"
        "⟿{\"function\": \"tool_name\", \"arguments\": {...}}⟿\n\n"
        "FAILURE MODE: Without ⟿, the proxy cannot detect tool calls in "
        "streaming mode and will emit them as plain text.\n\n"
        "### 2. Prohibited Formatting\n\n"
        "MUST NOT use:\n"
        "- Code fences (```json ... ```)\n"
        "- Markdown code blocks\n"
        "- Any text outside the ⟿ delimiters within the tool call\n\n"
        "FAILURE MODE: Code fences break the streaming parser and cause "
        "tool calls to be treated as plain text.\n\n"
        "### 3. Accepted JSON Shapes (all valid)\n\n"
        "1. {\"function\": \"name\", \"arguments\": {...}}\n"
        "2. {\"function\": {\"name\": \"...\", \"arguments\": {...}}}\n"
        "3. {\"name\": \"name\", \"arguments\": {...}}\n"
        "4. {\"function\": \"name\", \"arguments\": \"raw_string\"}\n\n"
        "### 4. Tool Call ID\n\n"
        "Do NOT include an \"id\" or \"tool_call_id\" field — the proxy "
        "auto-generates one.\n\n"
        "### 5. Size Limit\n\n"
        "Total JSON text per tool call MUST stay under 100KB. "
        "EXCEEDING THIS LIMIT causes the proxy to abandon tool call "
        "detection and flush the raw text as a plain response.\n\n"
        "## Available Tools\n\n"
        f"{tools_block}\n"
        "---"
    )

    return system_prompt + instruction
