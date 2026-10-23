# T-11 (remainder) - reasoning token count in `usage`

**Status:** partially done · **Effort:** S · **Depends on:** nothing
**Roadmap entry:** [`PLAN.md`](../../PLAN.md) Phase C

## What is already done

`choices[0].message.reasoning_content` is present in non-streaming replies
(`crates/proxy/src/main.rs:2107`), because the non-streaming path folds the same
`Channel::Reasoning` chunks the streaming path emits (ADR-0002). The original
gap - "non-streaming responses do not return reasoning" - is closed.

## What is missing

`usage.completion_tokens_details.reasoning_tokens`. OpenAI clients and billing
dashboards read it; today the field is absent, so a caller cannot tell how much
of `completion_tokens` was spent thinking.

## Design

The turn already counts reasoning text as it routes it. Two candidates:

1. **Count from the routed reasoning string** with the existing
   `estimate_tokens`, once per turn, at the point `usage` is assembled. Cheap,
   consistent with how the rest of `usage` is estimated, and the estimate is
   already documented as an estimate.
2. Take an upstream field if one exists. Verify first with a real non-streaming
   call that includes thinking; if DeepSeek sends its own count, prefer it and
   fall back to (1).

Either way the number is emitted only when thinking produced something, and
omitted (not zero) when thinking was off - a zero implies "thought, spent
nothing".

## Tests

- thinking on, non-streaming -> `usage.completion_tokens_details.reasoning_tokens > 0`
  and `reasoning_tokens < completion_tokens`
- thinking off -> the key is absent, not `0`
- streaming: `usage` in the final chunk carries the same value as the
  non-streaming path for the same fixture (`corpus/think.sse`)
- the value never exceeds `completion_tokens`

## Not in scope

Exact tokenization; per-choice reasoning counts (that is T-12).
