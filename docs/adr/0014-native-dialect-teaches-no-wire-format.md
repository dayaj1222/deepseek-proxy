# ADR-0014: The default dialect teaches no tool wire format

**Status:** accepted (2026-10-28)

## Context

Tool calls reach DeepSeek's web chat as plain text, so the proxy has to tell the
model how to write one and parse the reply. The first two dialects do this by
*teaching* a format: `json_invoke` (an invoke tag with a name attribute) and
`simple_tool_tag` (a one-marker tag around a JSON object).

The model already has a native spelling for tool calls (DSML tags). Teaching it
a different tag layout means the proxy's prompt steers the backend into
emitting markup that looks like the backend's own control syntax. We judge that
this is what draws policy attention to an account, and an account strike costs
far more than a slightly less tidy parser.

## Decision

`native` is the default dialect. Its prompt states only that tools exist, lists
their names and schemas, and says one call goes out per response. It names no
tag, marker or example. The model writes the call in its own spelling, and
`dialect_dsml` / `dialect_simple::native_at` read that back as the official
format. History replay and repair render calls with `dialect_dsml::render_tool_call`
so the transcript the model sees matches what it would have written itself.

`json_invoke` and `simple_tool_tag` stay selectable (`DIALECT` env, `[proxy]
dialect`) but are opt-in. `config.toml` leaves `dialect` unset so each binary
uses its own built-in default; an explicit value would break any binary that
does not know the name.

## Alternatives considered

- **Keep `json_invoke` as default.** Rejected: it teaches a concrete format,
  which is the exposure we are trying to remove.
- **Delete the older dialects.** Rejected by the owner: they are a fallback if
  the native spelling changes or misbehaves, and `json_invoke` is what some
  clients were built against.
- **Teach a format that is close to, but not, DSML.** Rejected: it keeps the
  risk and adds a lookalike to confuse the parser.

## Consequences

- The parser is more tolerant than the taught dialects because it reads
  whatever the model chooses; malformed output is handled by the repair ladder
  rather than prevented by a rigid prompt.
- **Known gap:** `Dialect::wire_shape` still returns a concrete DSML example for
  `Native`, and repair tiers 1 and 2 send it. That is the backend's own spelling
  rather than an invented one, but it is still a format shown to the model. Decide
  deliberately whether repair should drop it; until then this ADR holds for the
  normal prompt only.
- If the backend changes its native spelling, the reader breaks silently until
  repaired; the optional dialects are the escape hatch.
- Config reload cannot move a running turn between dialects (ADR-0005).

## Verification

`Dialect::default() == Native` (`config.rs` tests); the HTTP integration suite
runs on the default dialect, and two tests pin `DIALECT=json_invoke` to keep the
optional path working. `dialect_simple` tests assert the native prompt names no
syntax.
