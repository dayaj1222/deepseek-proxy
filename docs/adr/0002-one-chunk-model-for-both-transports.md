# ADR-0002: One chunk model feeds both transports

**Status:** accepted (2026-10-22)

## Context

`stream: false` is the *default* for every OpenAI SDK. The proxy answered it with
`text/event-stream`, which no SDK can deserialize, so the most ordinary call
there is looked broken. The fix had to produce a single `chat.completion` object
without duplicating retry, tool repair, cursor rewind or cursor persistence -
the four places where a turn is actually hard.

## Decision

A turn emits `sse::Chunk` values. The caller chooses the transport:
`sse_stream()` wraps them in SSE frames, `collect_completion()` folds them into
one `chat.completion` with `message.content`, `reasoning_content`, `tool_calls`
and `usage`.

Because both paths run the same turn function, a behaviour added to one is
automatically present in the other. In particular a non-streaming tool call now
arrives in `message.tool_calls` for the same reason a streaming one arrives as
deltas.

## Consequences

- Any future per-transport difference must be expressed as a fold over `Chunk`,
  not as a second code path. If a feature cannot be expressed that way, that is a
  design smell worth an ADR.
- Memory: the non-streaming path holds the whole answer. It is the client's
  request, so that is expected.
- Tests assert on both transports for behaviour that is supposed to be shared
  (for example `hello_turn()` is non-streaming; SSE assertions wrap it in the
  `streamed(...)` helper).
