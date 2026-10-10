"""One validation and recovery policy for both response transports."""

import json
from collections import Counter

from jsonschema import Draft202012Validator

from ..errors import ProxyError
from .format import build_repair_prompt


def fingerprint(call):
    fn = call["function"]
    return fn["name"], json.dumps(json.loads(fn["arguments"]), sort_keys=True, ensure_ascii=False)


class Recovery:
    def __init__(self, tools, required=False, parallel=True):
        self.tools = {t.function.name: t for t in tools}
        self.calls = []
        self.pending = []
        self.required = required
        self.parallel = parallel
        self.previous = Counter()
        self.repair_seen = Counter()
        self.expected = 0
        self.resolved = 0
        self.repairing = False

    def accept(self, event):
        if event.kind == "malformed":
            self.pending.append(event.value)
            return None
        if event.kind != "tool":
            return event
        call = event.value
        fn = call["function"]
        tool = self.tools.get(fn["name"])
        reason = None
        if tool is None:
            reason = "unknown_or_disallowed_tool"
        else:
            try:
                args = json.loads(fn["arguments"])
                if not isinstance(args, dict):
                    reason = "arguments_must_be_object"
                elif next(Draft202012Validator(tool.function.parameters).iter_errors(args), None):
                    reason = "arguments_do_not_match_schema"
            except (ValueError, TypeError):
                reason = "invalid_arguments_json"
        if reason:
            self.pending.append({"name": fn["name"], "reason": reason, "body": fn["arguments"]})
            return None
        key = fingerprint(call)
        if self.repairing:
            self.repair_seen[key] += 1
            if self.repair_seen[key] <= self.previous[key]:
                return None  # The repair echoed a previously accepted call.
        if not self.parallel and self.calls:
            raise ProxyError(
                "Backend produced multiple calls with parallel_tool_calls=false",
                code="tool_repair_failed",
            )
        self.calls.append(call)
        self.resolved += 1
        return event

    def require_call(self):
        if self.required and not self.calls and not self.pending:
            self.pending.append({"name": None, "reason": "required_tool_call_missing", "body": ""})

    def begin_repair(self):
        prompt = build_repair_prompt(self.pending, list(self.tools))
        prompt += "\nOnly fix the listed failures. Do not repeat calls already accepted."
        prompt += "\nAvailable schemas:\n" + json.dumps(
            [t.model_dump() for t in self.tools.values()], ensure_ascii=False
        )
        self.expected = len(self.pending)
        self.pending = []
        self.resolved = 0
        self.previous = Counter(fingerprint(c) for c in self.calls)
        self.repair_seen.clear()
        self.repairing = True
        return prompt

    def end_repair(self):
        # A repair attempt that resolved nothing must NOT re-accumulate the
        # same failures: doing so makes `begin_repair` grow `expected` on every
        # attempt and the failure list is re-listed in the prompt (and re-parsed
        # as new malformed events) until the retry budget is exhausted. Record
        # at most one omission marker per unresolved attempt.
        if self.resolved < self.expected:
            self.pending.append({"name": None, "reason": "repair_omitted_call", "body": ""})
