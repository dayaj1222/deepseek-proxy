# ADR-0001: An unterminated tool-call block is malformed, not prose

Status: accepted (2026-10-21)

## Context

Model output is parsed in two places: `dialect::scan` over a complete reply, and
`dialect::StreamToolFilter` over streamed chunks. They disagreed on one case —
a reply that ends inside an `<invoke ...>` block with no closing tag.

- `scan` reported `MalformedCall` with reason `unterminated_block`.
- `StreamToolFilter::finish` released the buffered bytes to the client as prose.

So the same bytes produced a repair turn in one path and a half-emitted tool
call in the client's transcript in the other.

## Decision

An unterminated block is **malformed**. It routes to the repair turn and never
reaches the client as content.

Both drivers now share one grammar (`dialect::Parser`, with `scan` as the
whole-reply reference), and `StreamToolFilter::finish` reports the trailing
block as `MalformedCall`. A trailing *partial tag* (`...<inv`) is still prose —
it is not yet a block.

## Consequences

- A reply cut off mid-block triggers one bounded repair turn instead of leaking
  a half-formed call. The turn is recoverable; the leak was not.
- `StreamToolFilter` now carries `malformed` alongside `calls`.
- `tests::parser_and_scan_agree_at_every_split` pins the two drivers to each
  other at every chunk boundary, so this class of disagreement cannot silently
  return.
