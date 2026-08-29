"""
Tool-call extraction and tool-description injection for the DeepSeek OpenAI proxy.

Canonical wire format (matches DeepSeek's training prior):

    <invoke name="tool_name">{"param": "value"}</invoke>

Legacy fallback accepted during migration:

    <invoke>{"function": "tool_name", "arguments": {...}}</invoke>

Both resolve to standard OpenAI tool_call objects downstream.
"""

import json
import logging
import re
import uuid
from typing import Any, Dict, List, Optional

from config import TOOL_CALL_TEMPLATE, TOOL_TAG_CLOSE, TOOL_TAG_OPEN

log = logging.getLogger(__name__)

# Regex for complete blocks. Group 1 = header attrs, group 2 = body.
_INVOKE_RE = re.compile(
    re.escape(TOOL_TAG_OPEN) + r"\b([^>]*)>(.*?)" + re.escape(TOOL_TAG_CLOSE),
    re.DOTALL,
)
# Unterminated block (output cut by token limit before </invoke>).
_UNTERMINATED_RE = re.compile(re.escape(TOOL_TAG_OPEN) + r"\b([^>]*)>(.*)\Z", re.DOTALL)
# name="..." or name='...' anywhere in the header; extra attrs ignored.
_NAME_ATTR_RE = re.compile(r"""\bname\s*=\s*(["'])([^"']+)\1""")

# Streaming handler flushes header as text beyond this length.
MAX_HEADER_LEN = 256

# XML format
_XML_PARAM_RE = re.compile(
    r"""<parameter\s+name\s*=\s*(["'])([^"']+)\1\s*>(.*?)</parameter>""",
    re.DOTALL,
)


def _parse_xml_params(body: str) -> Optional[Dict[str, Any]]:
    body = body.strip()
    if not body.startswith("<parameter"):
        return None
    params: Dict[str, Any] = {}
    for m in _XML_PARAM_RE.finditer(body):
        raw = m.group(3)
        try:
            params[m.group(2)] = json.loads(raw)  # handles "100" -> 100, "*" stays str
        except json.JSONDecodeError:
            params[m.group(2)] = raw
    return params or None


def extract_name_from_header(header: str) -> Optional[str]:
    """Pull the name attribute out of an <invoke ...> header. None if absent."""
    m = _NAME_ATTR_RE.search(header or "")
    return m.group(2).strip() if m else None


def _escape_raw_control_chars(s: str) -> str:
    """Escape raw control chars inside JSON strings as \\uXXXX. String-aware:
    leaves everything outside strings untouched."""
    out: List[str] = []
    in_str = False
    esc = False
    for ch in s:
        if not in_str:
            if ch == '"':
                in_str = True
            out.append(ch)
            continue
        if esc:
            esc = False
        elif ch == "\\":
            esc = True
        elif ch == '"':
            in_str = False
        elif ord(ch) < 0x20:
            out.append("\\u%04x" % ord(ch))
        out.append(ch)
    return "".join(out)


def _repair_json_object(s: str) -> Optional[str]:
    """Best-effort structural repair of a near-miss JSON object.

    Handles: missing closers (truncated output), stray closers in the tail.
    String-aware (braces inside quoted values are ignored). Returns None if
    the repaired result still doesn't parse.
    """
    s = _escape_raw_control_chars(s.strip())
    if not s.startswith("{"):
        return None
    out: List[str] = []
    depth = 0
    bd = 0
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
                break
        elif ch == "[":
            bd += 1
            out.append(ch)
        elif ch == "]":
            if bd > 0:
                bd -= 1
                out.append(ch)
        else:
            out.append(ch)
    if in_str:
        out.append('"')
    core = "".join(out)
    if not closed:
        core += "]" * bd + "}" * depth
    try:
        json.loads(core)
    except json.JSONDecodeError:
        return None
    return core


def _loads_tolerant(s: str) -> Optional[Any]:
    """json.loads with control-char normalization + structural repair."""
    try:
        return json.loads(s)
    except json.JSONDecodeError:
        repaired = _repair_json_object(s)
        if repaired is None:
            return None
        try:
            return json.loads(repaired)
        except json.JSONDecodeError:
            return None


def build_tool_call(name: Optional[str], body: str) -> Optional[Dict[str, Any]]:
    """Resolve (name, body) to an OpenAI tool_call, tolerantly. None if unusable.

    name given  -> body is the bare arguments JSON object (empty -> {}).
    name absent -> body is the legacy {"function": ..., "arguments": ...} object.
    """
    body = body.strip()
    if name:
        data: Any = _loads_tolerant(body) if body else {}
        if data is None:
            data = _parse_xml_params(body)
            if data is not None:
                log.info("Recovered XML-parameter payload for %s", name)

        if data is None:
            log.warning(
                "Dropped <invoke name=%r>: body is not valid JSON: %.300r", name, body
            )
            return None
        if not isinstance(data, dict):
            log.warning(
                "Dropped <invoke name=%r>: body is %s, expected a JSON object: %.300r",
                name,
                type(data).__name__,
                body,
            )
            return None
        return _to_tool_call({"function": name, "arguments": data})

    # Legacy shape (no name attribute)
    if not body:
        log.warning("Dropped <invoke>: no name attribute and empty body")
        return None
    data = _loads_tolerant(body)
    if isinstance(data, list):
        for item in data:
            if isinstance(item, dict) and isinstance(item.get("function"), str):
                return _to_tool_call(item)
        log.warning("Dropped <invoke>: list body with no usable call: %.300r", body)
        return None
    if not isinstance(data, dict):
        log.warning("Dropped <invoke>: body is not a JSON object: %.300r", body)
        return None
    if "function" not in data:
        log.warning(
            "Dropped <invoke>: missing name attribute AND no 'function' key: %.200r",
            body,
        )
        return None
    return _to_tool_call(data)


def extract_tool_calls(text: str) -> List[Dict[str, Any]]:
    """Extract tool calls from text — parses <invoke ...>...</invoke> blocks."""
    tool_calls: List[Dict[str, Any]] = []

    for m in _INVOKE_RE.finditer(text):
        tc = build_tool_call(extract_name_from_header(m.group(1)), m.group(2))
        if tc:
            tool_calls.append(tc)

    if not tool_calls and TOOL_TAG_OPEN in text:
        # Unterminated block salvage (token-limit cut before </invoke>).
        m = _UNTERMINATED_RE.search(text)
        if m:
            tc = build_tool_call(extract_name_from_header(m.group(1)), m.group(2))
            if tc:
                log.warning("Recovered unterminated <invoke> block")
                tool_calls.append(tc)
            else:
                log.warning("Unparseable unterminated <invoke>: %.300r", m.group(2))

    if tool_calls:
        log.debug("Extracted %d tool call(s)", len(tool_calls))
    return tool_calls


def parse_tool_call_json(json_str: str) -> Optional[Dict[str, Any]]:
    """Backwards-compatible wrapper: parse a bare payload (legacy shape)."""
    return build_tool_call(None, json_str)


def _to_tool_call(data: Dict[str, Any]) -> Optional[Dict[str, Any]]:
    """Convert {"function": name, "arguments": ...} to an OpenAI tool_call."""
    func_name = data.get("function")
    if not isinstance(func_name, str) or not func_name.strip():
        return None

    arguments = data.get("arguments")
    if arguments is None:
        arguments = {}

    if isinstance(arguments, dict):
        args_str = json.dumps(arguments, ensure_ascii=False)
    elif isinstance(arguments, str):
        try:
            json.loads(arguments)
            args_str = arguments
        except json.JSONDecodeError:
            log.warning(
                "arguments for %s was a non-JSON string; wrapped as {'input': ...}",
                func_name,
            )
            args_str = json.dumps({"input": arguments}, ensure_ascii=False)
    else:
        log.warning(
            "arguments for %s had type %s; coerced to JSON",
            func_name,
            type(arguments).__name__,
        )
        args_str = json.dumps(arguments, ensure_ascii=False)

    call_id = f"call_{uuid.uuid4().hex[:16]}"
    log.debug("Parsed tool call: %s  id=%s", func_name, call_id)
    return {
        "id": call_id,
        "type": "function",
        "function": {"name": func_name, "arguments": args_str},
    }


def inject_tool_descriptions(system_prompt: str, tools: List[Dict[str, Any]]) -> str:
    """Append tool definitions and usage instructions to the system prompt."""
    if not tools:
        return system_prompt

    tool_sections = []
    for tool in tools:
        if tool.get("type") != "function":
            continue
        func = tool.get("function") or {}
        name = func.get("name")
        if not name:
            continue
        params = json.dumps(func.get("parameters") or {}, indent=2, ensure_ascii=False)
        tool_sections.append(
            f"### {name}\n"
            f"Description: {func.get('description', 'No description provided.')}\n"
            f"Parameters (JSON Schema):\n```json\n{params}\n```"
        )

    tools_block = "\n\n".join(tool_sections)

    instruction = (
        "\n\n---\n"
        "## TOOL CALL PROTOCOL — MANDATORY, STRICT FORMAT\n\n"
        "To call a tool, emit a block in EXACTLY this format:\n\n"
        f"{TOOL_CALL_TEMPLATE}\n\n"
        "The tool name goes in the name attribute of the opening tag. "
        "The tool's parameters are the JSON object between the opening and "
        "closing tags.\n\n"
        "### Hard rules — violating any rule means your call is silently discarded\n"
        '1. ATTRIBUTES: the opening tag MUST carry a name attribute: <invoke name="tool_name">. '
        "Extra attributes (e.g. type) are ignored, but name is required.\n"
        "2. PAYLOAD: between the tags is ONE complete, valid JSON object containing "
        "the tool's parameters.\n"
        "3. CLOSURE: the JSON must be fully balanced — every { has its matching }. "
        "The last characters of every call are exactly:\n"
        "} </invoke>\n"
        "4. ESCAPING: escape all quotes, backslashes and newlines (\\n) inside JSON "
        "string values so the payload is valid JSON. NEVER write the literal text "
        '"</invoke>" inside a value.\n'
        '5. NO EXTRAS: never add an "id" field (auto-generated), never wrap the '
        "block in ``` code fences ```.\n"
        "6. MULTIPLE CALLS: emit multiple complete blocks back to back.\n"
        "7. You may write normal text before or after tool call blocks — but NEVER "
        "write text inside the tags, and never emit <invoke> without calling a tool.\n\n"
        "### Correct examples\n"
        '<invoke name="terminal">{"command": "ls -la"}</invoke>\n'
        '<invoke name="get_time">{}</invoke>\n\n'
        "### Wrong examples (all silently discarded)\n"
        '<invoke name="terminal">{"command": "ls"}</invoke>  <- truncated: missing final }\n'
        '<invoke>{"command": "ls"}</invoke>  <- missing name attribute\n'
        '<invoke name="terminal">command: ls</invoke>  <- payload must be a JSON object, not plain text\n\n'
        "## Available Tools\n\n"
        f"{tools_block}\n"
        "---\n"
        "FINAL REMINDER: tool calls use the form "
        f"{TOOL_CALL_TEMPLATE} — name in the attribute, JSON parameters between the tags."
    )

    return system_prompt + instruction
