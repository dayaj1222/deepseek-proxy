# ADR-0017: Native prompts may name the DSML format in words; the marker and markup stay out

**Status:** accepted (2026-10-31). Narrows the rule in ADR-0014; ADR-0014 is partly superseded by this ADR.
**Supersedes / amends:** ADR-0014 (the "names no tag, marker or example" rule only)

## Context

ADR-0014 keeps the native dialect from teaching a tool wire format, because
teaching the backend's own tag syntax is what risks an account strike. Its
decision text says the dialect "names no tag, marker or example". Read strictly,
that also forbids the word "DSML", and the native reminder and repair text were
written to avoid it.

The owner clarified on 2026-10-31 that the risk is the exact marker spelling and
concrete markup, not the name of the format. Naming "DSML" in plain words does
not teach a wire format.

## Decision

In `native` mode, prompts, reminders and repair text may name the format in
words, for example "DSML tool call". They must not contain:

- the marker spelling (the fullwidth vertical bar `｜`, the `DSML` token written
  between bars, or any `<`-tag);
- any concrete example of a call, tag or closer.

The native reminder and the repair text follow this. The marker check in tests
covers the bar character as well as the full marker, so the spelling cannot slip
in by another route.

## Alternatives considered

- **Keep the full ban on the word.** Rejected by the owner's clarification: the
  word alone is not the risk, and the ban pushed the prompts to vaguer wording
  that the model read less reliably.
- **Allow the marker in prompts too.** Rejected: that is the exposure ADR-0014
  exists to avoid.

## Consequences

- Easier: prompts can say what the format is called, which helps the model find
  its own spelling.
- Harder: nothing mechanical stops a future edit from adding the marker; the
  tests are the only guard.
- Risk knowingly accepted: the model can be told the format's name. The owner
  judged that harmless.
- Revisit this if account strikes correlate with the format's name appearing in
  prompts, or if the backend starts echoing the name back in tool calls.

## Verification

- `dialect_simple` test `teaches_no_wire_format_at_all` and `dialect` test
  `native_reminder_stays_short_and_vocabulary_free` used to ban the word "dsml".
  They now ban the marker token (`｜｜dsml｜｜`), the fullwidth bar, angle brackets
  and the ASCII pipe.
- `dialect` test `native_third_and_fourth_repairs_show_the_markerless_skeleton`
  requires the repair text to say "DSML tags yourself" and to omit the marker
  character.
