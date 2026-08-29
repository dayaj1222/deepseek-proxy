"""
Tool-call extraction and tool-description injection for the DeepSeek OpenAI proxy.

Canonical wire format (taught to the model via inject_tool_descriptions):

    <invoke name="tool_name">
    <parameter name="param">value</parameter>
    </invoke>

Fallbacks still parsed (model drift / older transcripts):

    <invoke name="tool_name">{"param": "value"}</invoke>            JSON payload
    <invoke>{"function": "tool_name", "arguments": {...}}</invoke>  legacy shape

Escape convention: "\\<marker>" is a literal marker; the proxy strips the
backslash. "\\X" is a literal backslash. Markers are masked to sentinels
before parsing and unmasked in final values.
"""

import json
import logging
from logger import get_logger
import re
import uuid
from typing import Any, Dict, List, Optional

from config import (
    TOOL_PARAM_CLOSE,
    TOOL_PARAM_OPEN,
    TOOL_TAG_CLOSE,
    TOOL_TAG_OPEN,
    render_prompt,
    settings,
)

log = get_logger(__name__)

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

# XML parameter format
_XML_PARAM_RE = re.compile(
    r"""<parameter\s+name\s*=\s*(["'])([^"']+)\1[^>]*>(.*?)</parameter>""",
    re.DOTALL,
)

# ---- Escape-mask machinery ----
# Sentinels stand in for escaped markers during parsing so real markers
# inside values can't confuse regexes or the streaming terminator.
_S_OPEN, _S_CLOSE = "\x00OI", "\x00CI"
_S_POPEN, _S_PCLOSE = "\x00OP", "\x00CP"
_BSLASH = "\x01"

ESC_TO_SENTINEL = {
    "\\" + TOOL_TAG_OPEN: _S_OPEN,
    "\\" + TOOL_TAG_CLOSE: _S_CLOSE,
    "\\" + TOOL_PARAM_OPEN: _S_POPEN,
    "\\" + TOOL_PARAM_CLOSE: _S_PCLOSE,
}
SENTINEL_TO_MARKER = {v: k for k, v in ESC_TO_SENTINEL.items()}


def mask_escapes(text: str) -> str:
    """Replace escaped markers with sentinels; '\\\\' -> literal-backslash sentinel."""
    for esc, sent in ESC_TO_SENTINEL.items():
        text = text.replace(esc, sent)
    return text.replace("\\\\", _BSLASH)


def unmask_markers(s: str) -> str:
    """Sentinels -> literal markers; backslash sentinel -> '\\'."""
    for sent, marker in SENTINEL_TO_MARKER.items():
        s = s.replace(sent, marker)
    return s.replace(_BSLASH, "\\")


def _escape_markers(s: str) -> str:
    """Inverse of the escape convention: prepend '\\' to any unescaped marker
    (used when re-serializing tool calls into prompt history)."""
    for marker in (TOOL_TAG_CLOSE, TOOL_TAG_OPEN, TOOL_PARAM_CLOSE, TOOL_PARAM_OPEN):
        s = re.sub(r"(?<!\\)" + re.escape(marker), "\\" + marker, s)
    return s


def extract_name_from_header(header: str) -> Optional[str]:
    """Pull the name attribute out of an <invoke ...> header. None if absent."""
    m = _NAME_ATTR_RE.search(header or "")
    return m.group(2).strip() if m else None


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
            params[m.group(2)] = unmask_markers(raw)
    return params or None


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


def _unmask_deep(o: Any) -> Any:
    """Recursively convert sentinels back to literal markers in parsed JSON."""
    if isinstance(o, str):
        return unmask_markers(o)
    if isinstance(o, list):
        return [_unmask_deep(v) for v in o]
    if isinstance(o, dict):
        return {k: _unmask_deep(v) for k, v in o.items()}
    return o


def build_tool_call(name: Optional[str], body: str) -> Optional[Dict[str, Any]]:
    """Resolve (name, body) to an OpenAI tool_call, tolerantly. None if unusable.

    name given  -> body is <parameter> XML or a bare arguments JSON object
                   (empty -> {}).
    name absent -> body is the legacy {"function": ..., "arguments": ...} object.
    """
    body = body.strip()
    if name:
        data: Any = _loads_tolerant(body) if body else {}
        if data is None:
            data = _parse_xml_params(body)
            if data is not None:
                log.debug("Parsed XML-parameter payload for %s", name)

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
    """Extract tool calls from text — parses <invoke ...>...</invoke> blocks.
    Escaped markers (\\<invoke etc.) are masked out first so they never match."""
    tool_calls: List[Dict[str, Any]] = []

    text = mask_escapes(text)

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


def _render_param_value(v: Any) -> str:
    """Render a Python value as raw <parameter> text (inverse of _parse_xml_params)."""
    if isinstance(v, str):
        return v
    if isinstance(v, bool):
        return "true" if v else "false"
    if v is None:
        return "null"
    if isinstance(v, (int, float)):
        return str(v)
    return json.dumps(v, ensure_ascii=False)  # lists / dicts


def format_tool_calls_for_history(tool_calls: List[Dict[str, Any]]) -> str:
    """Re-serialize OpenAI tool_call objects back into the wire format, so
    the model sees its own past calls in exactly the shape it is taught
    to emit (not the OpenAI JSON shape). Marker-escapes values on the way out."""
    blocks: List[str] = []
    for tc in tool_calls or []:
        if not isinstance(tc, dict):
            continue
        func = tc.get("function") or {}
        name = func.get("name")
        if not name:
            continue
        raw_args = func.get("arguments")
        try:
            args = json.loads(raw_args) if isinstance(raw_args, str) else raw_args
        except json.JSONDecodeError:
            args = None
        if isinstance(args, dict) and args:
            lines = [
                f'<parameter name="{k}">{_escape_markers(_render_param_value(v))}</parameter>'
                for k, v in args.items()
            ]
            blocks.append(
                f'<invoke name="{name}">\n' + "\n".join(lines) + "\n</invoke>"
            )
        else:
            blocks.append(f'<invoke name="{name}"></invoke>')
    return "\n".join(blocks)


def _to_tool_call(data: Dict[str, Any]) -> Optional[Dict[str, Any]]:
    """Convert {"function": name, "arguments": ...} to an OpenAI tool_call."""
    func_name = data.get("function")
    if not isinstance(func_name, str) or not func_name.strip():
        return None

    arguments = data.get("arguments")
    if arguments is None:
        arguments = {}

    if isinstance(arguments, dict):
        arguments = _unmask_deep(arguments)
        args_str = json.dumps(arguments, ensure_ascii=False)
    elif isinstance(arguments, str):
        arguments = unmask_markers(arguments)
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
    """Append tool definitions and usage instructions to the system prompt.

    All prompt text comes from config.toml [prompts]; the raw tool-call tag
    strings are substituted from Settings. Returns the system prompt unchanged
    when no tools are supplied.
    """
    if not tools:
        return system_prompt

    prompts = settings.prompts
    sections = []
    for tool in tools:
        if tool.get("type") != "function":
            continue
        func = tool.get("function") or {}
        name = func.get("name")
        if not name:
            continue
        params = json.dumps(func.get("parameters") or {}, indent=2, ensure_ascii=False)
        description = func.get("description") or "No description provided."
        sections.append(
            render_prompt(
                prompts.get(
                    "TOOL_SECTION",
                    "### {name}\nDescription: {description}\nParameters (JSON Schema):\n```json\n{params}\n```",
                ),
                name=name,
                description=description,
                params=params,
            )
        )

    tools_block = "\n\n".join(sections)

    instruction = prompts.get("TOOL_INSTRUCTION", "")
    instruction = render_prompt(instruction, tools_block=tools_block)

    header = prompts.get(
        "PROTOCOL_HEADER", "## TOOL CALL PROTOCOL — MANDATORY, STRICT FORMAT"
    )
    tools_header = prompts.get("TOOLS_HEADER", "## Available Tools")
    final_reminder = prompts.get("FINAL_REMINDER", "")
    final_reminder = render_prompt(final_reminder, tools_block=tools_block)

    block = (
        "\n\n---\n"
        f"{header}\n\n"
        f"{instruction}\n\n"
        f"{tools_header}\n\n"
        f"{tools_block}\n"
        "---\n"
        f"{final_reminder}"
    )
    return system_prompt + block
