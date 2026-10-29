# ADR-0014: The default dialect teaches no tool wire format

**Status:** accepted (2026-10-28), on trial: the owner is running it for several days before committing to it fully

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
- Repair in native mode also shows no format: `wire_shape` is a plain-language
  reminder ("emit the call in your native format") and the tier wording avoids
  tags. `native_repair_prompts_never_spell_out_markup` pins this.
- Config reload cannot move a running turn between dialects (ADR-0005).

## Amendment (2026-10-28): the per-turn reminder says "DSML"

The model sometimes answered with the old `simple_tool_tag` spelling, most
likely copying earlier turns of a thread that predates native. The reminder now
says "call a DSML tool". That names the backend's own format as an anchor
without spelling a tag, so it stays inside this ADR. Do not go further and name
or quote the legacy tag as a thing to avoid.

## Amendment (2026-10-29): repairs escalate from plain words to the call's shape

The per-turn reminder no longer says "DSML" and names no structure at all. When a
reply cannot be read, the repair prompts now give the model something to fill in
instead of "emit it in your native format", which left it guessing the syntax
after one slip and spiralling:

- repairs 1 and 2: the structure in plain words (outer call block, block naming
  the tool, block per argument, each closed by the closing marker of its own
  kind). No tag names, no markup, no DSML.
- repairs 3 and 4: the actual call shape for the actual tool, element words and
  argument names taken from its schema, with the marker left out, plus a short
  line telling the model to add the DSML tags itself.
- repair 5 and later: the full literal call with the real marker, to repeat
  verbatim. The last line of defence.

The markerless shape is the only place a repair shows angle brackets, and the
marker itself is built from `dialect_dsml::marker()`, never typed. The earlier
"do not go further" note is superseded for repairs 3+ only. Internal reason
codes (`dsml_unparsable`) and parser failure details are shown to the model in
neutral words. The parser also reads the model's usual slips by structure (a
bare closer closes the open parameter, `parameter="x"`, unquoted attributes,
a forgiven invoke closer), but does NOT read markerless tags: the model is told
to add the markers.

## Trial

Native is new and untested over long use. Until the owner closes the trial, do
not remove `json_invoke` or `simple_tool_tag`, and do not build features that
only work in native mode. To fall back: `DIALECT=json_invoke deepseek-rs restart`.
Record the outcome here (or supersede this ADR) when the trial ends.

## Verification

`Dialect::default() == Native` (`config.rs` tests); the HTTP integration suite
runs on the default dialect, and two tests pin `DIALECT=json_invoke` to keep the
optional path working. `dialect_simple` tests assert the native prompt names no
syntax.
