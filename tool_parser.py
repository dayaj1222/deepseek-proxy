import json
import re
import uuid
import logging
from typing import List, Dict, Any, Optional

log = logging.getLogger(__name__)

# ------------------------------------------------------------
# Tool call extraction
# ------------------------------------------------------------

def extract_tool_calls(text: str) -> List[Dict[str, Any]]:
    """
    Parse tool calls from plain-text model output.
    Returns a list of OpenAI-compatible tool_call objects.

    Recognised formats (in priority order):
    1. ```json { ... } ```  (markdown fenced)
    2. Bare JSON object containing "function" key at top level
    3. Loose balanced-brace scan (last resort)

    A JSON object is only treated as a tool call if it has a recognisable
    function name field — we never return arbitrary JSON blobs.
    """
    tool_calls: List[Dict[str, Any]] = []

    # ── 1. Markdown-fenced json blocks ──────────────────────────────────────
    fenced = re.findall(r'```(?:json)?\s*([\s\S]*?)\s*```', text)
    for block in fenced:
        parsed = _parse_block(block.strip())
        if parsed:
            tool_calls.extend(parsed)
            log.debug("Extracted %d tool call(s) from fenced block", len(parsed))

    if tool_calls:
        return tool_calls

    # ── 2. Bare JSON objects with "function" key ─────────────────────────────
    # Only match objects that have the word "function" inside them
    bare = re.findall(r'\{[^`]*?"function"[^`]*?\}', text, re.DOTALL)
    for candidate in bare:
        parsed = _parse_block(candidate.strip())
        if parsed:
            tool_calls.extend(parsed)

    if tool_calls:
        log.debug("Extracted %d tool call(s) from bare JSON", len(tool_calls))
        return tool_calls

    # ── 3. Balanced-brace scan (last resort) ────────────────────────────────
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
    """Try to parse a string as JSON and convert to OpenAI tool call format."""
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
    Convert a parsed dict to OpenAI tool_call format.

    Supported shapes:
      {"function": "name", "arguments": {...}}          ← model output format
      {"name": "name", "arguments": {...}}              ← alternative
      {"function": {"name": "...", "arguments": {...}}} ← OpenAI native format
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
        # Validate it's valid JSON; if not wrap it
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


# ------------------------------------------------------------
# Tool injection into system prompt
# ------------------------------------------------------------

def inject_tool_descriptions(system_prompt: str, tools: List[Dict[str, Any]]) -> str:
    """
    Append tool definitions to the system prompt.

    The instruction block tells the model exactly what JSON shape to emit,
    matching what _to_tool_call() expects on the way back.
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
        "## Tool Use Instructions\n\n"
        "You have access to the following tools. When you need to call a tool, "
        "respond with ONLY a JSON code block in this exact format and nothing else:\n\n"
        "```json\n"
        "{\"function\": \"tool_name\", \"arguments\": {\"param\": \"value\"}}\n"
        "```\n\n"
        "Rules:\n"
        "- Output ONLY the JSON block when calling a tool — no explanation before or after.\n"
        "- Do NOT guess or make up tool results. Wait for the tool result to be provided.\n"
        "- After receiving a tool result, continue with the task using that real result.\n"
        "- If multiple tools are needed, call them one at a time.\n\n"
        "## Available Tools\n\n"
        f"{tools_block}\n"
        "---"
    )

    return system_prompt + instruction
