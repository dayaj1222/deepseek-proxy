"""
Tool‑call extraction and tool‑description injection for the DeepSeek OpenAI proxy.
"""

import json
import logging
import re
import uuid
from typing import Any, Dict, List, Optional

log = logging.getLogger(__name__)


def extract_tool_calls(text: str) -> List[Dict[str, Any]]:
    """Extract tool calls from text — parses JSON wrapped in [TOOL CALL]...[/TOOL CALL]."""
    tool_calls: List[Dict[str, Any]] = []
    matches = re.findall(r'\[TOOL CALL\](.*?)\[/TOOL CALL\]', text, re.DOTALL)

    for match in matches:
        content = match.strip()
        if not content:
            continue
        tc = parse_tool_call_json(content)
        if tc:
            tool_calls.append(tc)

    if tool_calls:
        log.debug("Extracted %d tool call(s) from [TOOL CALL] markers", len(tool_calls))
    return tool_calls


def _repair_json_object(s: str) -> Optional[str]:
    """Best-effort structural repair of a near-miss JSON object.

    Handles DeepSeek's two observed failure modes without touching valid JSON:
    1. Missing closers (truncated output, e.g. outer ``}`` omitted before
       ``[/TOOL CALL]``) — appends the needed ``]``/``}``.
    2. Stray closers in the tail (e.g. ``"}]}}``) — drops ``]`` when no array
       is open and truncates past the top-level object's close.

    String-aware (braces inside quoted values are ignored). Returns None if
    the repaired result still doesn't parse.
    """
    s = s.strip()
    if not s.startswith("{"):
        return None
    out: List[str] = []
    depth = 0   # { } nesting
    bd = 0      # [ ] nesting
    in_str = False
    esc = False
    closed = False
    for ch in s:
        if in_str:
            out.append(ch)
            if esc:
                esc = False
            elif ch == "\\":
                esc = True
            elif ch == '"':
                in_str = False
            continue
        if ch == '"':
            in_str = True
            out.append(ch)
        elif ch == "{":
            depth += 1
            out.append(ch)
        elif ch == "}":
            depth -= 1
            out.append(ch)
            if depth == 0:
                closed = True
                break  # truncate anything after the top-level object closes
        elif ch == "[":
            bd += 1
            out.append(ch)
        elif ch == "]":
            if bd > 0:
                bd -= 1
                out.append(ch)
            # else: stray closer — drop it
        else:
            out.append(ch)
    if in_str:
        out.append('"')  # unterminated string — close it
    core = "".join(out)
    if not closed:
        core += "]" * bd + "}" * depth
    try:
        json.loads(core)
    except json.JSONDecodeError:
        return None
    return core


def parse_tool_call_json(json_str: str) -> Optional[Dict[str, Any]]:
    """Parse a JSON string and convert to an OpenAI tool_call object."""
    data = None
    try:
        data = json.loads(json_str)
    except json.JSONDecodeError:
        repaired = _repair_json_object(json_str)
        if repaired is None:
            return None
        try:
            data = json.loads(repaired)
        except json.JSONDecodeError:
            return None

    if isinstance(data, dict):
        return _to_tool_call(data)
    if isinstance(data, list):
        for item in data:
            if isinstance(item, dict):
                tc = _to_tool_call(item)
                if tc:
                    return tc
    return None


def _to_tool_call(data: Dict[str, Any]) -> Optional[Dict[str, Any]]:
    """Convert a parsed dictionary to an OpenAI tool_call object.

    Only one shape is accepted:
    * ``{"function": "<name>", "arguments": {...}}``
    """
    func_name = data.get("function")
    if not isinstance(func_name, str):
        return None

    arguments = data.get("arguments", {})

    if isinstance(arguments, dict):
        args_str = json.dumps(arguments)
    elif isinstance(arguments, str):
        try:
            json.loads(arguments)
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
    """Append tool definitions and usage instructions to the system prompt."""
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
        "## Tool Call Format — MANDATORY\n\n"
        "To call a tool, output exactly this and nothing else:\n"
        '[TOOL CALL]{"function": "tool_name", "arguments": {"param": "value"}}[/TOOL CALL]\n\n'
        "### Rules — a violation means your call is silently discarded\n"
        '1. Between the tags: ONE complete, balanced JSON object. Every "{" you open '
        'needs its matching "}". The tail of every call is exactly:\n'
        "} }[/TOOL CALL]\n"
        "(first } closes arguments, second } closes the object).\n"
        '2. Top-level keys are exactly "function" and "arguments". Raw parameters at '
        'top level (e.g. {"command": ...} alone) are INVALID.\n'
        "3. [/TOOL CALL] at the end is mandatory. Unclosed or truncated calls are dropped.\n"
        '4. Inside string values, escape quotes as \\\".\n'
        "5. NEVER use any other calling syntax — no XML, no <parameter>, "
        "<function_calls>, or similar. Only this bracket format works here.\n"
        "6. Multiple calls: emit multiple complete [TOOL CALL]...[/TOOL CALL] blocks "
        "back to back, with no text between them.\n"
        "7. Do NOT include an \"id\" field — the proxy auto-generates one.\n\n"
        "### Correct example\n"
        '[TOOL CALL]{"function": "terminal", "arguments": {"command": "ls -la"}}[/TOOL CALL]\n\n'
        "### Wrong examples\n"
        '[TOOL CALL]{"function": "terminal", "arguments": {"command": "ls"}[/TOOL CALL]  <- missing final }\n'
        '[TOOL CALL]{"command": "ls"}[/TOOL CALL]  <- missing function/arguments wrapper\n\n'
        "## Available Tools\n\n"
        f"{tools_block}\n"
        "---"
    )

    return system_prompt + instruction
