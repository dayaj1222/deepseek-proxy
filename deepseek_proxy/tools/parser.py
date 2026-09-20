"""Incremental tool syntax parser. No transport, validation, or repair turns.

Text values are strings, tool values are OpenAI tool-call dictionaries, and
malformed values are dictionaries with name/body/reason. Adjacent text event
boundaries are unspecified. Each block produces at most one outcome.
Consecutive orphan parameters (including separating whitespace) form one
malformed group, held until a non-parameter boundary or EOF.
"""

from dataclasses import dataclass
import json
import re
from typing import Any

from . import format as fmt


@dataclass
class Event:
    kind: str
    value: Any


_OPEN = re.compile(fmt._INVOKE_OPEN_PAT)
_PARAM = re.compile(fmt._PARAM_OPEN_PAT)
_CLOSE = re.compile(fmt._INVOKE_CLOSE_PAT)
_PCLOSE = re.compile(fmt._PARAM_CLOSE_PAT)
_WRAPPER = re.compile(fmt._WRAPPER_PAT)


def _dsml_escape_prefix(candidate):
    """Whether an escape can still become a whitespace-tolerant DSML tag."""
    if not candidate.startswith("\\<"):
        return False
    rest = candidate[2:]
    if rest.startswith("/"):
        rest = rest[1:]
    if not rest:
        return True
    if not rest.startswith("｜"):
        return False
    rest = rest.lstrip("｜").lstrip()
    if "DSML".startswith(rest):
        return True
    if not rest.startswith("DSML"):
        return False
    rest = rest[4:].lstrip()
    if not rest:
        return True
    if not rest.startswith("｜"):
        return False
    rest = rest.lstrip("｜").lstrip()
    return any(
        name.startswith(rest) or (rest.startswith(name) and rest[len(name) :].isspace())
        for name in ("invoke", "parameter", "tool_calls", "function_calls", "calls")
    )


class ToolParser:
    def __init__(self, enabled=True, buffer_limit=100000):
        if not isinstance(buffer_limit, int) or buffer_limit < 1:
            raise ValueError("buffer_limit must be a positive integer")
        self.enabled = enabled
        self.buffer_limit = buffer_limit
        self._mode = "text"
        self._body = ""
        self._header = ""
        self._tag = ""
        self._escape = ""
        self._name = None
        self._json = None
        self._literal_param = False
        self._quoted = False
        self._slash = False
        self._overflow = False
        self._done = False
        self._literal_markers = {}

    def _restore(self, value):
        if isinstance(value, str):
            for token, marker in self._literal_markers.items():
                value = value.replace(token, marker)
        elif isinstance(value, list):
            value = [self._restore(item) for item in value]
        elif isinstance(value, dict):
            value = {self._restore(key): self._restore(item) for key, item in value.items()}
        return value

    def _emit(self, events, kind, value):
        if kind == "tool" and self._literal_markers:
            function = value["function"]
            function["arguments"] = json.dumps(
                self._restore(json.loads(function["arguments"])), ensure_ascii=False
            )
        elif kind == "malformed":
            value = self._restore(value)
        if kind == "text" and not value:
            return
        if kind == "text" and events and events[-1].kind == "text":
            events[-1].value += value
        else:
            events.append(Event(kind, value))

    def _malformed(self, events, reason, body):
        self._emit(
            events,
            "malformed",
            {
                "name": self._name,
                "body": fmt.unmask_markers(body.strip()),
                "reason": reason,
            },
        )

    def _append(self, value, events):
        if self._mode == "orphan_gap" and not value.isspace():
            self._resolve(events)
        if self._mode == "text":
            self._emit(events, "text", value)
            return
        if not self._overflow:
            remaining = self.buffer_limit - len(self._body) - len(self._header)
            self._body += value[: max(0, remaining)]
            if len(value) > remaining:
                self._malformed(events, "buffer_limit_exceeded", self._header + self._body)
                self._body = self._header = ""
                self._overflow = True

    def _reset(self):
        self._mode = "text"
        self._body = self._header = ""
        self._name = None
        self._json = None
        self._literal_param = False
        self._quoted = self._slash = self._overflow = False
        self._literal_markers.clear()

    def _resolve(self, events, eof=False):
        if not self._overflow:
            if self._mode == "tool":
                if eof and not self._body.strip():
                    self._malformed(events, "truncated_at_eof", self._body)
                else:
                    call, reason = fmt.build_tool_call_detailed(self._name, self._body)
                    if call:
                        self._emit(events, "tool", call)
                    else:
                        self._malformed(events, reason or "truncated_at_eof", self._body)
            elif self._mode == "mislabel":
                calls = fmt._salvage_mislabeled_invoke(self._header + self._body)
                if calls:
                    for call in calls:
                        self._emit(events, "tool", call)
                else:
                    self._malformed(
                        events,
                        "orphan_parameter_unterminated" if eof else "orphan_parameter_block",
                        self._header + self._body,
                    )
            elif self._mode in ("orphan", "orphan_gap"):
                self._malformed(
                    events,
                    "orphan_parameter_unterminated"
                    if eof and self._mode == "orphan"
                    else "orphan_parameter_block",
                    self._header + self._body,
                )
        self._reset()

    def _handle_tag(self, tag, events):
        if self._mode == "orphan_gap":
            if _PARAM.match(tag) and not fmt._MISLABEL_TOOL_RE.search(tag):
                self._mode = "orphan"
                self._append(tag, events)
                return
            self._resolve(events)
        if self._mode == "mislabel":
            if _WRAPPER.fullmatch(tag) or _OPEN.match(tag):
                self._resolve(events)
            else:
                self._append(tag, events)
                return
        if self._mode == "tool":
            if _CLOSE.fullmatch(tag):
                self._resolve(events)
            elif _OPEN.match(tag):
                self._resolve(events, eof=True)
                self._handle_tag(tag, events)
            else:
                self._append(tag, events)
                if _PARAM.match(tag):
                    self._json = None
                    self._literal_param = bool(re.search(r"""\bstring\s*=\s*(["'])true\1""", tag))
                elif _PCLOSE.fullmatch(tag):
                    self._json = False
                    self._literal_param = False
            return
        if self._mode == "orphan":
            if _OPEN.match(tag):
                self._resolve(events, eof=True)
                self._handle_tag(tag, events)
                return
            self._append(tag, events)
            if _PCLOSE.fullmatch(tag):
                self._mode = "orphan_gap"
            return
        if _WRAPPER.fullmatch(tag):
            return
        if _OPEN.match(tag) or _PARAM.match(tag):
            self._mode = "tool" if _OPEN.match(tag) else "orphan"
            self._name = fmt.extract_name_from_header(tag) if self._mode == "tool" else None
            if self._mode == "orphan" and fmt._MISLABEL_TOOL_RE.search(tag):
                self._mode = "mislabel"
            self._header = tag
            if len(tag) > self.buffer_limit:
                self._malformed(events, "buffer_limit_exceeded", tag[: self.buffer_limit])
                self._header = ""
                self._overflow = True
        else:
            self._emit(events, "text", tag)

    def _char(self, char, events):
        if self._tag:
            if char == "<":
                self._append(self._tag, events)
                self._tag = "<"
                return
            self._tag += char
            if char == ">":
                tag, self._tag = self._tag, ""
                self._handle_tag(tag, events)
            elif len(self._tag) > min(fmt.MAX_HEADER_LEN, self.buffer_limit):
                tag, self._tag = self._tag, ""
                if self._mode == "text" and (_OPEN.match(tag) or _PARAM.match(tag)):
                    self._handle_tag(tag, events)
                    if not self._overflow:
                        self._malformed(events, "header_limit_exceeded", tag)
                        self._overflow = True
                        self._header = ""
                else:
                    self._append(tag, events)
            return
        if char == "<" and not self._quoted:
            self._tag = char
            return
        if self._mode == "tool":
            if self._json is None and not char.isspace():
                self._json = not self._literal_param and char in '{["'
            if self._json:
                if self._slash:
                    self._slash = False
                elif self._quoted and char == "\\":
                    self._slash = True
                elif char == '"':
                    self._quoted = not self._quoted
        self._append(char, events)

    def feed(self, text: str) -> list[Event]:
        if self._done:
            raise ValueError("cannot feed a finished parser")
        if not isinstance(text, str):
            raise TypeError("feed expects str")
        if not self.enabled:
            return [Event("text", text)] if text else []
        events = []
        for char in text:
            if self._escape:
                candidate = self._escape + char
                marker = candidate[1:]
                dsml_marker = "｜" in marker and any(
                    pattern.fullmatch(marker)
                    for pattern in (_OPEN, _PARAM, _CLOSE, _PCLOSE, _WRAPPER)
                )
                if candidate in fmt.ESC_TO_SENTINEL or dsml_marker:
                    if self._mode == "orphan_gap":
                        self._resolve(events)
                    value = (
                        candidate[1:]
                        if self._mode == "text"
                        else fmt.ESC_TO_SENTINEL.get(candidate)
                    )
                    if value is None:
                        value = f"\ue000literal{len(self._literal_markers)}\ue001"
                        if not self._overflow:
                            self._literal_markers[value] = marker
                    if self._mode == "tool" and self._json and self._slash:
                        # The previous backslash belongs to a JSON escape
                        # pair, not the wire-format marker escape.
                        self._append("\\", events)
                        self._slash = False
                    self._append(value, events)
                    self._escape = ""
                elif len(candidate) <= fmt.MAX_HEADER_LEN and (
                    any(marker.startswith(candidate) for marker in fmt.ESC_TO_SENTINEL)
                    or _dsml_escape_prefix(candidate)
                ):
                    self._escape = candidate
                else:
                    pending, self._escape = self._escape, ""
                    for literal in pending:
                        self._char(literal, events)
                    if char == "\\":
                        self._escape = char
                    else:
                        self._char(char, events)
            elif char == "\\" and not self._tag:
                self._escape = char
            else:
                self._char(char, events)
        return events

    def finish(self) -> list[Event]:
        if self._done:
            return []
        self._done = True
        events = []
        for char in self._escape:
            self._char(char, events)
        self._escape = ""
        if self._tag:
            tag, self._tag = self._tag, ""
            if self._mode == "orphan_gap" and _PARAM.match(tag):
                self._mode = "orphan"
            if self._mode == "text" and (_OPEN.match(tag) or _PARAM.match(tag)):
                self._name = fmt.extract_name_from_header(tag) if _OPEN.match(tag) else None
                self._malformed(
                    events,
                    "header_truncated" if _OPEN.match(tag) else "orphan_parameter_header_truncated",
                    tag,
                )
            elif self._mode == "tool" and any(
                close.startswith(tag) for close in fmt.INVOKE_CLOSE_VARIANTS
            ):
                pass  # A partial closing tag adds no argument data.
            else:
                self._append(tag, events)
        if self._mode != "text":
            self._resolve(events, eof=True)
        return events
