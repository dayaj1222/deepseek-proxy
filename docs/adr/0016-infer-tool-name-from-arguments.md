# ADR-0016: A call that drops its tool name runs only if its arguments fit exactly one offered tool

**Status:** accepted (2026-10-31)

## Context

The backend sometimes writes a tool call's arguments without the tool block
that names the tool. The reply has `calls` open, then `parameter` blocks, and no
`invoke` tag. The parser had no name to give the call, so it reported "an
argument block appeared outside any tool block" and the repair turn asked the
model to send it again.

Over 152 raw replies from two captures (2026-10-30 and 2026-10-31, see
`docs/analysis/malformed-tool-calls.md`), this shape was the largest single
failure. Replayed through the parser at `main`, 97 replies produced no call.
Of those, 78 events were this nameless shape. Almost all of them carried only
`command` (which maps to `bash`) or only `edits` (which maps to `edit`). In the
invoke-tagged replies those argument names belong to one tool each.

Repair is not free. Of 284 settled repair attempts, 137 resolved nothing, and
each attempt costs a full model turn.

## Decision

When a DSML block has arguments but no tool name, and it is the only block in its
reply, the proxy runs the call if exactly one offered tool fits it. A tool fits
when its schema declares every argument given and requires none that are
missing. If zero or two or more tools fit, the block stays a malformed call, whose reason
now says the tool name is missing, and the repair turn runs.

The check lives in `dialect::infer_tool_name`. It runs in `Recovery::accept`,
where the offered tools are known, and the inferred call then goes through the
normal schema validation and the parallel-call check. Blocks that are truncated,
or that sit beside another block in the same reply, are never inferred.

This applies to every dialect that reads DSML, including `simple_tool_tag`,
because both share `dialect_simple::scan`.

## Alternatives considered

- **Leave it to the repair turn.** Rejected: it costs a model turn, and the
  settled data shows repair resolves nothing in 137 of 284 attempts.
- **Map argument names to tool names in a fixed table** (`command` → `bash`).
  Rejected: it hard-codes a client's tool names. A client with a different `bash`
  or a second tool that also takes `command` would be run wrongly.
- **Pick the first tool that fits when more than one does.** Rejected: it runs a
  tool the model may not have meant, and nothing in the reply separates the two.
- **Infer from truncated blocks too.** Rejected: a cut-off command may be partial,
  and running a partial command is worse than a repair turn.
- **Teach the model to always name the tool.** Rejected by ADR-0014: it would
  teach a wire format.

## Consequences

- Easier: about 57 of the 152 captured replies now produce a call that used to
  need a repair turn. No reply that already parsed changed its calls.
- Harder: a call can run a tool the model did not name. The risk we accept is
  narrow: it happens only when exactly one offered tool's schema fits the
  argument names. A permissive schema on a second tool makes the match ambiguous,
  and then nothing is run.
- The inferred name is logged at `info` with the tool name only. Arguments are
  not logged.
- Revisit this if the backend starts naming tools in most calls (the rule then
  matters less), or if a client sends schemas that declare few properties (a
  tool with no `properties` is never a candidate, so the rule under-matches
  rather than over-matches).

## Verification

- Unit: `dialect::infer_tool_name` tests in `infer_tests` (`the_one_tool_whose_schema_fits_is_inferred`,
  `an_ambiguous_match_is_not_inferred`, `a_missing_required_or_unknown_argument_is_not_inferred`).
- Parser: `dialect_dsml` tests `arguments_with_no_invoke_tag_are_kept_for_inference`,
  `nameless_arguments_are_not_inferred_next_to_another_block`,
  `a_parameter_after_a_closed_invoke_is_not_a_nameless_call`,
  `a_truncated_nameless_block_is_never_inferred`.
- End to end, through `Dialect::Native.scan` then `Recovery::accept`:
  `a_nameless_reply_runs_the_one_tool_its_arguments_fit`,
  `a_nameless_reply_that_fits_no_tool_stays_malformed`,
  `a_nameless_reply_matching_two_tools_is_not_run`. The first fails before this
  change, because the reply had no call.
- Corpus probe, run once and not kept in the repo: 55 replies parsed before and
  all 55 produce the same calls now. 57 more produce a call through inference
  (29 bash, 26 edit, 1 codemode, 1 read). The probe used approximate schemas
  for eight tools, because the real request schemas are not in the corpus. Re-run
  it with the real schemas before trusting the 57.
