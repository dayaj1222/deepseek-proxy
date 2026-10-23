# T-12 - `n > 1`

**Status:** planned · **Effort:** M · **Depends on:** T-08 (a choice must be able to stop)
**Roadmap entry:** [`PLAN.md`](../../PLAN.md) Phase C

## Problem

`n` is ignored, so a caller that asks for three choices gets one and pays for
one. Unlike a sampling parameter, `n` changes *how many* answers arrive, and a
client that indexes `choices[1]` gets a panic or an empty string.

## Design constraint: one thread, one cursor

The whole thread model assumes a linear conversation with a single
`parent_message_id`. Running `n` choices on the same remote session would fork
the cursor: choice 1 advances it, choice 2 then answers against choice 1's
context, and the next turn's parent is ambiguous.

**So each choice after the first runs on a sibling session** - a new remote
session for the same thread, so the conversation continues correctly but the
primary cursor is untouched. Only choice 0 commits to the thread binding and
cursor, which is the one the next turn will continue from.

## Surface

- `n: u32`, default 1, capped by `[limits] max_n` (default 4). Above the cap ->
  400 with `param: "n"`, not a silent clamp.
- Streaming with `n > 1` -> 400 `unsupported_combination`. Interleaving N streams
  correctly (index-tagged deltas, one `[DONE]`) is a protocol problem worth doing
  properly or not at all.
- `usage` sums all choices.

## Tests

- `n: 2` non-streaming -> two choices, distinct `index`, both complete
- only choice 0 advances the thread cursor (assert `parents()` after the turn)
- next turn continues from choice 0's answer
- `n` above `max_n` -> 400
- streaming with `n: 2` -> 400
- `usage.completion_tokens` is the sum, not the last choice

## Documented costs

- `n` multiplies the rate-limit budget: N turns against one shared gate. The
  README says so, because a `n: 4` habit is how an account gets throttled.
- Choices are not independent samples of one prompt state if the prompt carried
  images: the image is uploaded once and referenced by file id by every sibling.
  That is cheaper and is also a small deviation from "independent".

## Not in scope

Streaming `n > 1`, per-choice tools, per-choice cursors persisted across turns.

## Risks

- **Cursor forking** is the reason this is M rather than S. The test that only
  choice 0 commits is the guard.
- **Cost surprise**: N turns where the client expected one. Documented and
  capped.
