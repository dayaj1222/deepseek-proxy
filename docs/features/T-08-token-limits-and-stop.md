# T-08 - `max_tokens`, `max_completion_tokens`, `stop`

**Status:** planned · **Effort:** M · **Depends on:** nothing
**Roadmap entry:** [`PLAN.md`](../../PLAN.md) Phase C

## Problem

`ChatRequest` silently ignores all three. A client that sets `max_tokens: 100`
gets a 2000-token answer and pays for it. A client that sets `stop: ["\n\n"]`
gets the rest of the document. Silence is the worst option: the caller cannot
tell "not supported" from "did not need to stop".

## Constraint

The upstream cannot be told these parameters. Everything is enforced proxy-side,
which means the proxy must be able to *stop reading and emitting* without leaving
a stale cursor.

## Surface

- `max_tokens: u32` and `max_completion_tokens: u32` on `ChatRequest`.
  `max_completion_tokens` wins when both are present (OpenAI semantics); both are
  validated as `>= 1` with `param` set on the error.
- `stop: String | Vec<String>` (max 4 entries, each non-empty) validated in
  `validate_request`.

## Design

**Stop matching across chunk boundaries.** Hold back `max(len(stop)) - 1`
trailing bytes, exactly like `partial_tag_suffix` in `dialect.rs`. Without it a
stop string split over two SSE frames is missed.

**Prose only, never inside a tool block.** A truncated tool call is worse than no
tool call: the client cannot execute a partial JSON body, and the repair path
would then spend a turn on a self-inflicted error. If a block has started, the
block finishes and the turn stops afterwards.

**Finish reasons.** Stop match -> `finish_reason: "stop"`. Token cap ->
`finish_reason: "length"`. The cap uses the existing `estimate_tokens`
(tiktoken), so it is an estimate - the README says so rather than implying an
exact model tokenizer.

**The cursor must still advance.** Truncating the client view must not leave a
stale `parent_message_id`; the turn commits the cursor it received, then stops
emitting. Cancelling the upstream read is the same mechanism the disconnect path
already uses.

## Tests

- stop split across two SSE chunks matches once
- stop inside `reasoning_content` is ignored
- cap hit mid-prose -> `finish_reason: "length"`, no partial UTF-8
- cap hit during a tool block -> the block completes, then the turn stops
- non-streaming equivalents for all of the above
- `parent_message_id` advanced after an early stop (the risk-register test)
- `max_completion_tokens` overrides `max_tokens`
- `stop` with 5 entries -> 400 with `param: "stop"`

## Not in scope

Exact model tokenization. Upstream-side limits (the proxy cannot send them).

## Risks

- **Stale cursor** is the top risk in `PLAN.md`; it gets a dedicated test.
- **UTF-8**: holding back N bytes must not split a multi-byte character. Hold
  back on a `char` boundary, and assert with a multi-byte stop string.
