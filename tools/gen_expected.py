#!/usr/bin/env python3
"""Generate routing expectations for the Rust port from captured SSE streams.

This runs the *Python* router (`deepseek_proxy.thinking`) over each
`corpus/*.sse` file and writes `corpus/<name>.expected.json`, which the Rust
test suite asserts against byte-for-byte.

Usage (from the repo root):

    ../deepseek/.venv/bin/python tools/gen_expected.py

The Python implementation is the oracle. If Python and Rust disagree, the
Rust port is wrong until proven otherwise.
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
PY_PROXY = REPO.parent / "deepseek"
sys.path.insert(0, str(PY_PROXY))

from deepseek_proxy.thinking import _FragmentRouter, _route_event  # noqa: E402

CORPUS = REPO / "corpus"


def route(raw: str) -> list[dict]:
    """Replay an SSE stream through the Python router."""
    router = _FragmentRouter()
    out: list[dict] = []

    for line in raw.splitlines():
        line = line.strip()
        if not line.startswith("data:"):
            continue
        payload = line[5:].strip()
        if not payload or payload == "[DONE]":
            continue
        try:
            event = json.loads(payload)
        except json.JSONDecodeError:
            continue
        for channel, text in _route_event(event, router):
            if text:
                out.append({"channel": channel, "text": text})

    return out


def main() -> int:
    files = sorted(CORPUS.glob("*.sse"))
    if not files:
        print(f"no .sse files in {CORPUS}", file=sys.stderr)
        return 1

    for path in files:
        events = route(path.read_text())
        target = path.with_suffix(".expected.json")
        target.write_text(json.dumps(events, indent=0) + "\n")

        reasoning = sum(len(e["text"]) for e in events if e["channel"] == "reasoning")
        text = sum(len(e["text"]) for e in events if e["channel"] == "text")
        print(
            f"{path.name}: {len(events)} events "
            f"(reasoning={reasoning} chars, text={text} chars) -> {target.name}"
        )

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
