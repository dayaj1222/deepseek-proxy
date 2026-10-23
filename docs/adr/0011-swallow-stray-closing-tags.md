# ADR-0011: Up to two stray closing tags are swallowed

**Status:** accepted (2026-10-23)

## Context

Models sometimes close a tool block two or three times. The extra closing tag was
reaching the client as prose, so a well-formed call produced a reply with visible
markup after it.

## Decision

After a completed block, up to `MAX_STRAY_CLOSERS` (2) closing tags are dropped.
Anything past the limit is emitted verbatim.

All three consumers apply the same rule through one helper, `strip_stray_closers`:
`extract_tool_calls` (non-streaming parse), `scan` (routing), and the streaming
`Parser` (which must also *hold back* bytes it has not yet decided on).

## Consequences

- A hard limit rather than "all of them" is deliberate: a model quoting the tag
  in prose (this project's own prompts do) must still be able to emit it. Two
  covers observed model behaviour; three or more is prose.
- The streaming path drains from `self.pending` rather than from the emitted
  buffer, so the limit applies to bytes not yet shown to the client.
- Both the swallow and the beyond-limit case have tests, because the second is
  the one a future "simplification" would remove.
