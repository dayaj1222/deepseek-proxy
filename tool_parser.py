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


def parse_tool_call_json(json_str: str) -> Optional[Dict[str, Any]]:
    """Parse a JSON string and convert to an OpenAI tool_call object."""
    try:
        data = json.loads(json_str)
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
        "## Tool Call Format\n\n"
        "Wrap every tool call with the tags:\n"
        "[TOOL CALL]{\"function\": \"tool_name\", \"arguments\": {...}}[/TOOL CALL]\n\n"
        "You may emit multiple tool calls in a single response — "
        "each one wrapped in its own pair of tags.\n"
        "Only pure JSON between the tags — no text, no markdown, no extra whitespace.\n"
        "Do NOT include an \"id\" field — the proxy auto-generates one.\n"
        "Only the format above is accepted.\n\n"
        "## Available Tools\n\n"
        f"{tools_block}\n"
        "---"
    )

    return system_prompt + instruction
