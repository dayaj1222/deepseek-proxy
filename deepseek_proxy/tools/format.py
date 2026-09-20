"""Single source of truth for the tool-call wire format.

Everything that governs the format lives here and ONLY here:

  - the DIALECTS registry (tag spellings, body kind, instruction text)
  - the one-switch selection (TOOL_FORMAT env var or config.toml key)
  - emit helpers (template, history serialization, prompt injection)
  - parse helpers (extraction, tolerant JSON, escape masking)
  - streaming helpers (open/close matchers re-exported for the SSE handler)

To swap formats, change ONE value — ``TOOL_FORMAT`` — to a key of DIALECTS:

    TOOL_FORMAT = "json_invoke"   # <invoke name="t">{...json...}</invoke>
    TOOL_FORMAT = "xml_params"    # <invoke name="t"><parameter ...>...</invoke>
    TOOL_FORMAT = "dsml"          # legacy <｜DSML｜invoke ...> dialect

Parsing stays universal on purpose: every known spelling is accepted no
matter which dialect is taught, so old transcripts keep working. The active
dialect controls only what we EMIT / TEACH.

Canonical wire format per dialect is documented on each Dialect below.
Escape convention (all dialects): "\\<marker>" is a literal marker; the
proxy strips the backslash. Markers are masked to sentinels before parsing
and unmasked in final values.
"""

from __future__ import annotations

import json
import os
import re
import tomllib
import uuid
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Dict, List, Optional

import logging as _logging

# NOTE: stdlib logging here, NOT logger.get_logger — logger imports config,
# which reads this module's active_name() at startup (import cycle).
log = _logging.getLogger(__name__)

# ---------------------------------------------------------------------------
# 1. Dialect registry — THE one place that defines formats.
# ---------------------------------------------------------------------------

ROOT = Path.cwd()


@dataclass(frozen=True)
class Dialect:
    """One teachable/parseable tool-call dialect.

    body_kind "json":       <invoke name="t">{...json args...}</invoke>
    body_kind "xml_params": <invoke name="t"><parameter name="p" ...>v</parameter>...</invoke>
    Empty wrapper_* means no wrapper lines are emitted.
    """

    name: str
    invoke_open: str
    invoke_close: str
    param_open: str = ""
    param_close: str = ""
    wrapper_open: str = ""
    wrapper_close: str = ""
    body_kind: str = "json"
    instruction: str = ""
    final_reminder: str = ""
    format_reminder: str = ""

    def template(self) -> str:
        """Concrete example block shown to the model in prompts."""
        if self.body_kind == "xml_params":
            lines = (
                [
                    f"{self.wrapper_open}" if self.wrapper_open else None,
                    f'{self.invoke_open} name="tool_name">',
                    f'{self.param_open} name="param1" string="true">value1{self.param_close}',
                    f'{self.param_open} name="count" string="false">5{self.param_close}',
                    f"{self.invoke_close}",
                    f"{self.wrapper_close}" if self.wrapper_close else None,
                ],
            )
            return "\n".join(line for line in lines[0] if line)
        body = '{"param1": "value1", "count": 5}'
        core = f'{self.invoke_open} name="tool_name">{body}{self.invoke_close}'
        if self.wrapper_open:
            return f"{self.wrapper_open}\n{core}\n{self.wrapper_close}"
        return core


DIALECTS: Dict[str, Dialect] = {
    "json_invoke": Dialect(
        name="json_invoke",
        invoke_open="<invoke",
        invoke_close="</invoke>",
        body_kind="json",
        instruction="""To call a tool, emit a block in EXACTLY this format:

{tool_call_template}

The tool name goes in the name attribute of the opening tag. Between the
tags put ONE JSON object holding all arguments. Each property in a tool's
Parameters (JSON Schema) below corresponds to one key of that object.

### Required tool-call rules
1. ATTRIBUTES: the opening tag MUST carry a name attribute:
   TOOL_CALL_OPEN name="tool_name". Extra attributes are ignored, but name
   is required.
2. BODY: exactly one JSON object between the tags. Strings, numbers,
   booleans, null, arrays and nested objects are all fine as JSON values.
   Do NOT wrap the block in code fences, never emit bare JSON with no tags.
3. LITERAL MARKERS: if a value must contain the literal text TOOL_CALL_OPEN
   or TOOL_CALL_CLOSE, write a backslash directly before it.
4. MULTIPLE CALLS: emit multiple complete blocks back to back.
5. You may write normal text before or after tool call blocks.
6. CALL ONLY WHEN NEEDED: if no tool fits, answer directly with no block.
   After a tool result arrives, continue the task — do not re-emit the
   same call.
7. NO GUESSING: use only parameters from the schema below; never invent values.
""",
        final_reminder="""FINAL REMINDER: tool calls use exactly this form:

{tool_call_template}

name in the TOOL_CALL_OPEN attribute; one JSON object between the tags.""",
        format_reminder="""[FORMAT REMINDER] Tool calls must use exactly this shape:
{tool_call_template}
Tool name in the TOOL_CALL_OPEN name attribute; one JSON object between the tags. Malformed calls require correction before execution.""",
    ),
    "xml_params": Dialect(
        name="xml_params",
        invoke_open="<invoke",
        invoke_close="</invoke>",
        param_open="<parameter",
        param_close="</parameter>",
        body_kind="xml_params",
        instruction="""To call a tool, emit a block in EXACTLY this format:

{tool_call_template}

The tool name goes in the name attribute of the opening TOOL_CALL_OPEN tag.
Each argument is one TOOL_PARAM_OPEN element: the parameter name goes in its
name attribute, and the value is the raw text between TOOL_PARAM_OPEN and
TOOL_PARAM_CLOSE. Each property in a tool's Parameters (JSON Schema) below
corresponds to one TOOL_PARAM_OPEN element.

### Required tool-call rules
1. ATTRIBUTES: the opening tag MUST carry a name attribute:
   TOOL_CALL_OPEN name="tool_name". Extra attributes (e.g. type) are ignored,
   but name is required.
2. PARAMETERS: every argument MUST be its own
   TOOL_PARAM_OPEN name="..." string="true|false">valueTOOL_PARAM_CLOSE element. Do NOT put a JSON
   object between the TOOL_CALL_OPEN tags.
   string="true" = plain text (keep as-is); string="false" = JSON value
   (numbers, booleans, null, arrays, objects). When in doubt use "true".
   Concrete example: TOOL_PARAM_OPEN name="location" string="true">ParisTOOL_PARAM_CLOSE.
3. VALUES: the text between TOOL_PARAM_OPEN tags is taken literally.
4. LITERAL MARKERS: if a value must contain the literal text TOOL_CALL_OPEN,
   TOOL_CALL_CLOSE, TOOL_PARAM_OPEN or TOOL_PARAM_CLOSE, write a backslash
   directly before it.
5. NO EXTRAS: never add an id field, never wrap the block in code fences, never emit bare JSON.
6. MULTIPLE CALLS: emit multiple complete blocks back to back inside one wrapper.
7. You may write normal text before or after tool call blocks.
8. CALL ONLY WHEN NEEDED: if no tool fits, answer directly with no block.
   After a tool result arrives, continue the task — do not re-emit the same call.
9. NO GUESSING: use only parameters from the schema below; never invent values.
""",
        final_reminder="""FINAL REMINDER: tool calls use exactly this form:

{tool_call_template}

name in the TOOL_CALL_OPEN attribute; one TOOL_PARAM_OPEN name="...">valueTOOL_PARAM_CLOSE element per argument.""",
        format_reminder="""[FORMAT REMINDER] Tool calls must use exactly this shape:
{tool_call_template}
Tool name in the TOOL_CALL_OPEN name attribute; one TOOL_PARAM_OPEN name="...">valueTOOL_PARAM_CLOSE element per argument. Malformed calls require correction before execution.""",
    ),
    "dsml": Dialect(
        name="dsml",
        invoke_open="<｜DSML｜invoke",
        invoke_close="</｜DSML｜invoke>",
        param_open="<｜DSML｜parameter",
        param_close="</｜DSML｜parameter>",
        wrapper_open="<｜DSML｜tool_calls>",
        wrapper_close="</｜DSML｜tool_calls>",
        body_kind="xml_params",
        instruction="",  # replaced below: dsml shares the xml_params prose
        final_reminder="",
        format_reminder="",
    ),
}

# dsml shares the xml_params prose (same body kind, different tags).
_D = DIALECTS["dsml"]
DIALECTS["dsml"] = Dialect(
    name=_D.name,
    invoke_open=_D.invoke_open,
    invoke_close=_D.invoke_close,
    param_open=_D.param_open,
    param_close=_D.param_close,
    wrapper_open=_D.wrapper_open,
    wrapper_close=_D.wrapper_close,
    body_kind=_D.body_kind,
    instruction=DIALECTS["xml_params"].instruction,
    final_reminder=DIALECTS["xml_params"].final_reminder,
    format_reminder=DIALECTS["xml_params"].format_reminder,
)
del _D


# ---------------------------------------------------------------------------
# 2. One-switch selection: TOOL_FORMAT env var > config.toml > default.
# ---------------------------------------------------------------------------

DEFAULT_DIALECT = "json_invoke"


def _toml_tool_format() -> Optional[str]:
    # Resolve the path at call time.  This keeps the helper correct for
    # callers that set DEEPSEEK_CONFIG after importing the package (tests,
    # embedding applications, and reloaders), while retaining the normal
    # settings loader as the fallback.
    configured_path = os.getenv("DEEPSEEK_CONFIG")
    if configured_path:
        try:
            with open(configured_path, "rb") as handle:
                value = tomllib.load(handle).get("TOOL_FORMAT")
        except FileNotFoundError:
            value = None
        except tomllib.TOMLDecodeError as exc:
            raise ValueError(f"Invalid TOML configuration in {configured_path}") from exc
    else:
        from ..settings import _TOML

        value = _TOML.get("TOOL_FORMAT")
    return str(value).strip() if value else None


def active_name() -> str:
    """Resolve the active dialect name. Unknown values fall back to default."""
    from ..settings import settings

    raw = settings.tool_format.strip()
    if raw not in DIALECTS:
        log.warning("Unknown TOOL_FORMAT %r — falling back to %r", raw, DEFAULT_DIALECT)
        return DEFAULT_DIALECT
    return raw


ACTIVE: Dialect = DIALECTS[active_name()]

# Emit aliases (what we TEACH). Parse lists below accept every known
# spelling regardless of the active dialect.
TOOL_TAG_OPEN = ACTIVE.invoke_open
TOOL_TAG_CLOSE = ACTIVE.invoke_close
TOOL_PARAM_OPEN = ACTIVE.param_open
TOOL_PARAM_CLOSE = ACTIVE.param_close
TOOL_WRAPPER_OPEN = ACTIVE.wrapper_open
TOOL_WRAPPER_CLOSE = ACTIVE.wrapper_close
TOOL_CALL_TEMPLATE = ACTIVE.template()
TOOL_CALL_PREFIX = TOOL_TAG_OPEN
TOOL_CALL_SUFFIX = TOOL_TAG_CLOSE


# ---------------------------------------------------------------------------
# 3. Universal parse tables — accept every known spelling, always.
# ---------------------------------------------------------------------------


def _variants(configured: str, *known: str) -> list[str]:
    seen: list[str] = []
    for v in (configured, *known):
        if v and v not in seen:
            seen.append(v)
    return sorted(seen, key=len, reverse=True)


_DSML1_INVOKE = "<｜DSML｜invoke"
_DSML2_INVOKE = "<｜｜DSML｜｜invoke"
_DSML1_PARAM = "<｜DSML｜parameter"
_DSML2_PARAM = "<｜｜DSML｜｜parameter"

INVOKE_OPEN_VARIANTS = _variants(TOOL_TAG_OPEN, _DSML1_INVOKE, _DSML2_INVOKE, "<invoke")
INVOKE_CLOSE_VARIANTS = _variants(
    TOOL_TAG_CLOSE, "</｜DSML｜invoke>", "</｜｜DSML｜｜invoke>", "</invoke>"
)
PARAM_OPEN_VARIANTS = _variants(TOOL_PARAM_OPEN, _DSML1_PARAM, _DSML2_PARAM, "<parameter")
PARAM_CLOSE_VARIANTS = _variants(
    TOOL_PARAM_CLOSE, "</｜DSML｜parameter>", "</｜｜DSML｜｜parameter>", "</parameter>"
)
# Outer wrappers carry no data; stripped before parsing.
WRAPPER_VARIANTS = _variants(
    TOOL_WRAPPER_OPEN,
    "<｜DSML｜tool_calls>",
    "<｜DSML｜function_calls>",
    "<｜｜DSML｜｜tool_calls>",
)

# Whitespace-tolerant patterns: the model mixes single/double-pipe DSML
# spellings AND inserts spaces ("<｜｜ DSML｜｜ parameter").
# ｜ = U+FF5C fullwidth pipe.
_DSML_TOK = r"｜+\s*DSML\s*｜+"
_INVOKE_OPEN_PAT = r"(?:<invoke|<" + _DSML_TOK + r"\s*invoke)\b"
_INVOKE_CLOSE_PAT = r"(?:</invoke\s*>|</" + _DSML_TOK + r"\s*invoke\s*>)"
_PARAM_OPEN_PAT = r"(?:<parameter|<" + _DSML_TOK + r"\s*parameter)\b"
_PARAM_CLOSE_PAT = r"(?:</parameter\s*>|</" + _DSML_TOK + r"\s*parameter\s*>)"
_WRAPPER_PAT = (
    r"(?:"
    + r"<"
    + _DSML_TOK
    + r"\s*(?:tool_calls|function_calls|calls)\s*>|</"
    + _DSML_TOK
    + r"\s*(?:tool_calls|function_calls|calls)\s*>)"
)
_WRAPPER_RE = re.compile(_WRAPPER_PAT)

_INVOKE_RE = re.compile(
    _INVOKE_OPEN_PAT + r"([^>]*)>(.*?)" + _INVOKE_CLOSE_PAT,
    re.DOTALL,
)
# Unterminated block (output cut by token limit before close tag).
_UNTERMINATED_RE = re.compile(_INVOKE_OPEN_PAT + r"([^>]*)>(.*)\Z", re.DOTALL)
# name="..." or name='...' anywhere in the header; extra attrs ignored.
_NAME_ATTR_RE = re.compile(r"""\bname\s*=\s*(["'])([^"']+)\1""")

# Streaming handler flushes header as text beyond this length.
MAX_HEADER_LEN = 256

# DSML/native parameter format. Captures name, string-flag, value.
# string="true" -> keep raw string; "false"/absent -> try JSON, fall back raw.
_XML_PARAM_RE = re.compile(
    _PARAM_OPEN_PAT
    + r"""\s+name\s*=\s*(["'])([^"']+)\1[^>]*?(?:string\s*=\s*(["'])(true|false)\3)?[^>]*>(.*?)"""
    + _PARAM_CLOSE_PAT,
    re.DOTALL,
)

# ---- Escape-mask machinery ----
# Sentinels stand in for escaped markers during parsing so real markers
# inside values can't confuse regexes or the streaming terminator.
_S_OPEN, _S_CLOSE = "\x00OI", "\x00CI"
_S_POPEN, _S_PCLOSE = "\x00OP", "\x00CP"
_BSLASH = "\x01"


def _build_escape_map() -> dict[str, str]:
    m: dict[str, str] = {}
    opens = INVOKE_OPEN_VARIANTS + PARAM_OPEN_VARIANTS + WRAPPER_VARIANTS
    closes = INVOKE_CLOSE_VARIANTS + PARAM_CLOSE_VARIANTS
    for i, v in enumerate(opens):
        m["\\" + v] = f"\x00O{i:02d}"
    for i, v in enumerate(closes):
        m["\\" + v] = f"\x00C{i:02d}"
    return m


ESC_TO_SENTINEL = _build_escape_map()
SENTINEL_TO_MARKER = {v: k for k, v in ESC_TO_SENTINEL.items()}


def mask_escapes(text: str) -> str:
    """Replace escaped markers with sentinels; '\\\\' -> literal-backslash sentinel."""
    for esc, sent in ESC_TO_SENTINEL.items():
        text = text.replace(esc, sent)
    return text.replace("\\\\", _BSLASH)


def unmask_markers(s: str) -> str:
    """Sentinels -> literal markers; backslash sentinel -> '\\'."""
    for sent, marker in SENTINEL_TO_MARKER.items():
        s = s.replace(sent, marker[1:])
    return s.replace(_BSLASH, "\\")


_ALL_MARKERS = (
    INVOKE_OPEN_VARIANTS
    + INVOKE_CLOSE_VARIANTS
    + PARAM_OPEN_VARIANTS
    + PARAM_CLOSE_VARIANTS
    + WRAPPER_VARIANTS
)


def _escape_markers(s: str) -> str:
    """Inverse of the escape convention: prepend '\\' to any unescaped marker
    (used when re-serializing tool calls into prompt history)."""
    for marker in sorted(set(_ALL_MARKERS), key=len, reverse=True):
        s = re.sub(r"(?<!\\)" + re.escape(marker), "\\" + marker, s)
    return s


def extract_name_from_header(header: str) -> Optional[str]:
    """Pull the name attribute out of an <invoke ...> header. None if absent."""
    m = _NAME_ATTR_RE.search(header or "")
    return m.group(2).strip() if m else None


def _coerce_json_value(val: str) -> tuple[bool, Any]:
    """Best-effort JSON parse of a string-flagged parameter value.

    Models emit Python-style \\' escapes (illegal in JSON) and raw control
    chars; normalize both before parsing. Returns (parsed_ok, value) where
    failure carries the normalized raw string.
    """
    cand = val.replace("\\'", "'")  # \\' is never valid JSON — safe to fold
    data = _loads_tolerant(cand)
    if data is not None:
        return True, data
    return False, cand


def _parse_xml_params(body: str) -> Optional[Dict[str, Any]]:
    body = body.strip()
    if "<parameter" not in body and "｜" not in body:
        return None
    params: Dict[str, Any] = {}
    position = 0
    for m in _PARAM_OPEN_ATTRS_RE.finditer(body):
        if m.start() < position:
            continue
        name = extract_name_from_header(m.group(1))
        if not name:
            continue
        flag_match = re.search(r"""\bstring\s*=\s*(["'])(true|false)\1""", m.group(1))
        flag = flag_match.group(2) if flag_match else ""
        start = m.end()
        quoted = escaped = False
        json_value = flag != "true" and body[start:].lstrip().startswith(("{", "[", '"'))
        close = None
        for index in range(start, len(body)):
            char = body[index]
            if json_value:
                if escaped:
                    escaped = False
                    continue
                if quoted and char == "\\":
                    escaped = True
                    continue
                if char == '"':
                    quoted = not quoted
            if char == "<" and not quoted:
                close = _PARAM_CLOSE_FIND_RE.match(body, index)
                if close:
                    break
        if close is None:
            continue
        position = close.end()
        raw = body[start : close.start()]
        val = unmask_markers(raw)
        if flag == "true":
            params[name] = val
        else:
            # "false" or absent (plain-XML dialect): typed value expected.
            _, params[name] = _coerce_json_value(val)
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
            continue
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
    stack: List[str] = []
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
        elif ch in "{[":
            stack.append("}" if ch == "{" else "]")
            out.append(ch)
        elif ch in "}]":
            if ch not in stack:
                continue
            while stack and stack[-1] != ch:
                out.append(stack.pop())
            out.append(stack.pop())
            if not stack:
                closed = True
                break
        else:
            out.append(ch)
    if in_str:
        if esc:
            out.append("\\")
        out.append('"')
    core = "".join(out)
    if not closed:
        core += "".join(reversed(stack))
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
            log.warning("Dropped <invoke name=%r>: body is not valid JSON: %.300r", name, body)
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


def build_tool_call_detailed(name, body):
    """Same as build_tool_call but also returns a machine-usable failure reason.

    Returns (tool_call|None, reason|None). reason is None on success, else a
    short code with detail, e.g. "missing_name", "invalid_json:...".
    """
    b = body.strip()
    if name:
        data = _loads_tolerant(b) if b else {}
        if data is None:
            data = _parse_xml_params(b)
        if data is None:
            return None, "invalid_json_body: %.300r" % b
        if not isinstance(data, dict):
            return None, "body_not_object: got %s: %.300r" % (type(data).__name__, b)
        return _to_tool_call({"function": name, "arguments": data}), None
    if not b:
        return None, "missing_name_and_empty_body"
    data = _loads_tolerant(b)
    if isinstance(data, list):
        for item in data:
            if isinstance(item, dict) and isinstance(item.get("function"), str):
                return _to_tool_call(item), None
        return None, "list_body_no_usable_call: %.300r" % b
    if not isinstance(data, dict):
        return None, "body_not_object_no_name: %.300r" % b
    if "function" not in data:
        return None, "missing_name_no_function_key: %.200r" % b
    return _to_tool_call(data), None


def extract_malformed(text):
    """Records for <invoke> blocks that fail to parse: name/body/reason."""
    out = []
    masked = mask_escapes(text)
    seen = set()
    for m in _INVOKE_RE.finditer(masked):
        tc, reason = build_tool_call_detailed(extract_name_from_header(m.group(1)), m.group(2))
        if tc is None:
            key = (m.group(1), m.group(2))
            if key not in seen:
                seen.add(key)
                out.append(
                    {
                        "name": extract_name_from_header(m.group(1)),
                        "body": unmask_markers(m.group(2).strip()),
                        "reason": reason,
                    }
                )
    # Models sometimes emit a bare DSML parameter and omit the enclosing
    # invoke/tool name. It must enter repair handling instead of being
    # mistaken for ordinary assistant text.
    invoke_spans = [(m.start(), m.end()) for m in _INVOKE_RE.finditer(masked)]
    for m in _XML_PARAM_RE.finditer(masked):
        if any(start <= m.start() < end for start, end in invoke_spans):
            continue
        key = ("orphan_parameter", m.group(0))
        if key not in seen:
            seen.add(key)
            out.append(
                {
                    "name": None,
                    "body": unmask_markers(m.group(0).strip()),
                    "reason": "orphan_parameter_block",
                }
            )
    if not out and any(v in masked for v in INVOKE_OPEN_VARIANTS):
        m = _UNTERMINATED_RE.search(masked)
        if m:
            tc, reason = build_tool_call_detailed(extract_name_from_header(m.group(1)), m.group(2))
            if tc is None:
                out.append(
                    {
                        "name": extract_name_from_header(m.group(1)),
                        "body": unmask_markers(m.group(2).strip()),
                        "reason": reason or "unterminated_block",
                    }
                )
    return out


def build_repair_prompt(malformed, tool_names=None, include_bad_body=False):
    """Build a correction-turn prompt without replaying the bad assistant turn.

    Repair turns are sent on the same DeepSeek conversation as the failed
    response.  The backend therefore already has the malformed assistant
    output in its conversation history.  Repeating each bad body here makes
    the model see ``bad call + correction instructions`` twice and increases
    the chance that it echoes the bad block.  The optional body is retained
    for callers that use this helper outside that conversation context.
    """
    lines = [
        "Your previous response contained malformed tool call(s) that could "
        "not be executed. The failed response is already in the conversation "
        "history. Do not quote, copy, explain, or repeat it. Re-emit EACH "
        "corrected call now, and nothing else."
    ]
    for i, m in enumerate(malformed, 1):
        # Parser reasons may contain a repr of the entire malformed body
        # (for diagnostics). Keep that detail in logs/records, but never put
        # it into a same-thread repair prompt where it would replay the bad
        # call. The model only needs the stable failure category.
        reason = str(m.get("reason") or "malformed_tool_call").split(":", 1)[0]
        failure = "Failure %d: name=%r reason=%s" % (i, m.get("name"), reason)
        if include_bad_body:
            failure += "\nBad body:\n%s" % ((m.get("body") or "")[:1500])
        lines.append(failure)
    if tool_names:
        lines.append("Valid tool names: %s." % ", ".join(tool_names))
    lines.append(
        "Re-emit each fixed call in EXACTLY this shape:\n"
        + ACTIVE.template()
        + '\nRules: opening tag MUST carry name="tool_name"; '
        + (
            "body is ONE JSON object with all arguments. "
            if ACTIVE.body_kind == "json"
            else f'body contains one {ACTIVE.param_open} name="argument" '
            f'string="true|false">value{ACTIVE.param_close} element per argument. '
            'Use string="true" for literal text and string="false" for JSON values. '
        )
        + "Emit corrected blocks back to back, no "
        "chatter, no code fences."
    )
    return "\n".join(lines)


_MISLABEL_TOOL_RE = re.compile(r"invoke\s+name\s*=\s*[\"']?([\w.\-]+)", re.IGNORECASE)
_MISLABEL_PARAM_RE = re.compile(r"parameter\s+name\s*=\s*[\"']?([\w.\-]+)", re.IGNORECASE)
_PARAM_OPEN_ATTRS_RE = re.compile(_PARAM_OPEN_PAT + r"([^>]*?)>")
_PARAM_CLOSE_FIND_RE = re.compile(_PARAM_CLOSE_PAT)


def _salvage_mislabeled_invoke(text: str) -> List[Dict[str, Any]]:
    """Last resort: model emitted <parameter name="invoke name="TOOL">...
    (invoke/parameter levels confused, quotes nested). Rebuild one call from
    the mislabeled parameter tags. Only called when normal parsing found
    nothing, so it cannot shadow valid blocks.
    """
    opens = list(_PARAM_OPEN_ATTRS_RE.finditer(text))
    if not opens:
        return []
    tool: Optional[str] = None
    params: Dict[str, Any] = {}
    for i, om in enumerate(opens):
        attrs = om.group(1)
        if tool is None:
            mt = _MISLABEL_TOOL_RE.search(attrs)
            if mt:
                tool = mt.group(1)
            continue
        mp = _MISLABEL_PARAM_RE.search(attrs)
        if not mp:
            continue
        pname = mp.group(1)
        start = om.end()
        mc = _PARAM_CLOSE_FIND_RE.search(text, start)
        mend = len(text)
        for later in opens[i + 1 :]:
            if later.start() >= start:
                mend = min(mend, later.start())
                break
        if mc and mc.start() < mend:
            mend = mc.start()
        raw_val = unmask_markers(text[start:mend].strip())
        mflag = re.search(r"string\s*=\s*[\"'](true|false)[\"']", attrs, re.IGNORECASE)
        if mflag and mflag.group(1).lower() == "true":
            params[pname] = raw_val
        else:
            _, params[pname] = _coerce_json_value(raw_val)
    if tool and params:
        log.warning("Recovered mislabeled invoke-as-parameter block for %s", tool)
        tc = _to_tool_call({"function": tool, "arguments": params})
        return [tc] if tc else []
    return []


def extract_tool_calls(text: str) -> List[Dict[str, Any]]:
    """Extract tool calls from text — parses <invoke ...>...</invoke> blocks.
    Escaped markers (\\<invoke etc.) are masked out first so they never match."""
    tool_calls: List[Dict[str, Any]] = []

    text = mask_escapes(text)

    for m in _INVOKE_RE.finditer(text):
        tc = build_tool_call(extract_name_from_header(m.group(1)), m.group(2))
        if tc:
            tool_calls.append(tc)

    if not tool_calls:
        salvaged = _salvage_mislabeled_invoke(text)
        if salvaged:
            return salvaged

    if not tool_calls and any(v in text for v in INVOKE_OPEN_VARIANTS):
        # Unterminated block salvage (token-limit cut before </invoke>).
        m = _UNTERMINATED_RE.search(text)
        if m:
            tc = build_tool_call(extract_name_from_header(m.group(1)), m.group(2))
            if tc:
                log.info(
                    "Recovered unterminated <invoke name=%r>; no repair required",
                    extract_name_from_header(m.group(1)),
                )
                tool_calls.append(tc)
            else:
                log.warning("Unparseable unterminated <invoke>: %.300r", m.group(2))

    if tool_calls:
        log.debug("Extracted %d tool call(s)", len(tool_calls))
    return tool_calls


def parse_tool_call_json(json_str: str) -> Optional[Dict[str, Any]]:
    """Backwards-compatible wrapper: parse a bare payload (legacy shape)."""
    return build_tool_call(None, json_str)


def _render_param_value(v: Any) -> tuple[str, str]:
    """Render a Python value as (text, string-flag) for <parameter>."""
    if isinstance(v, str):
        return v, "true"
    if isinstance(v, bool):
        return ("true" if v else "false"), "false"
    if v is None:
        return "null", "false"
    if isinstance(v, (int, float)):
        return str(v), "false"
    return json.dumps(v, ensure_ascii=False), "false"  # lists / dicts


def format_tool_calls_for_history(tool_calls: List[Dict[str, Any]]) -> str:
    """Re-serialize OpenAI tool_call objects back into the ACTIVE wire format,
    so the model sees its own past calls in exactly the shape it is taught
    to emit (not the OpenAI JSON shape). Marker-escapes values on the way out."""
    d = ACTIVE
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
        open_tag = f'{d.invoke_open} name="{name}">'
        if isinstance(args, dict) and args:
            if d.body_kind == "json":
                body = _escape_markers(json.dumps(args, ensure_ascii=False))
                core = f"{open_tag}{body}{d.invoke_close}"
            else:
                lines = []
                for k, v in args.items():
                    text, flag = _render_param_value(v)
                    lines.append(
                        f'{d.param_open} name="{k}" string="{flag}">{_escape_markers(text)}{d.param_close}'
                    )
                core = open_tag + "\n" + "\n".join(lines) + f"\n{d.invoke_close}"
        else:
            core = f"{open_tag}{d.invoke_close}"
        if d.wrapper_open:
            blocks.append(f"{d.wrapper_open}\n{core}\n{d.wrapper_close}")
        else:
            blocks.append(core)
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


def _placeholder_map() -> Dict[str, str]:
    """Placeholder token -> active tag string, for prompt prose written with
    TOOL_CALL_OPEN / TOOL_CALL_CLOSE / TOOL_PARAM_OPEN / TOOL_PARAM_CLOSE."""
    return {
        "TOOL_CALL_CLOSE": ACTIVE.invoke_close,
        "TOOL_CALL_OPEN": ACTIVE.invoke_open,
        "TOOL_PARAM_CLOSE": ACTIVE.param_close,
        "TOOL_PARAM_OPEN": ACTIVE.param_open,
    }


def render_instruction(text: str, **ctx: Any) -> str:
    """Render dialect instruction prose: substitute tag placeholders, then
    {placeholders} (tool_call_template defaults to the active template)."""
    from ..settings import render_prompt

    ctx.setdefault("tool_call_template", ACTIVE.template())
    ctx.setdefault("tools_block", "")
    return render_prompt(text, **ctx)


def inject_tool_descriptions(system_prompt: str, tools: List[Dict[str, Any]]) -> str:
    """Append tool definitions and usage instructions to the system prompt.

    Instruction/reminder prose comes from the ACTIVE dialect; the per-tool
    section template and headers come from config.toml [prompts] with
    built-in fallbacks. Returns the system prompt unchanged when no tools
    are supplied.
    """
    if not tools:
        return system_prompt

    from ..settings import settings

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
            render_instruction(
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
    instruction = render_instruction(ACTIVE.instruction, tools_block=tools_block)
    final_reminder = render_instruction(ACTIVE.final_reminder, tools_block=tools_block)

    header = prompts.get("PROTOCOL_HEADER", "## TOOL CALL PROTOCOL — MANDATORY, STRICT FORMAT")
    tools_header = prompts.get("TOOLS_HEADER", "## Available Tools")

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


def format_reminder_text() -> str:
    """Periodic re-anchoring reminder, rendered from the ACTIVE dialect."""
    return render_instruction(ACTIVE.format_reminder)
