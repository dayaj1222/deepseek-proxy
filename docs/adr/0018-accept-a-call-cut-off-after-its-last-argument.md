# ADR-0018: A call cut off after its last whole argument is accepted

**Status:** accepted (2026-10-31)

## Context

The backend often stops a tool call after its last argument, before it writes
the closing tags for the invoke block and the calls wrapper. The reply then ends
with a complete argument and nothing after it.

The parser reported these as "the output ended before the call was closed" and
sent them to repair. In the 152 captured replies, 24 zero-call replies had this
shape. Every argument in them was complete. Only the closers were missing.

A reply can also be cut off in the middle of an argument, because of the token
limit. That case must still fail: the value may be partial.

## Decision

When the reply ends with an invoke that has a tool name, at least one argument,
and no argument still open, the parser accepts the call. Otherwise the call
stays a failure, with the same reason as before. In particular, a cut-off in the
middle of an argument, a cut-off with no argument, and a nameless call are never
accepted here.

The accepted call goes through the normal schema check in `Recovery::accept`.
If a required argument is missing because the reply was cut off earlier, the
call fails validation and the repair turn runs.

The rule lives in `parse_region`, in the code that handles the end of the text.

## Alternatives considered

- **Leave it to the repair turn.** Rejected: repair costs a model turn, and the
  model repeats the same cut-off most of the time.
- **Accept any cut-off call, including one cut mid-argument.** Rejected: a
  truncated argument may be partial, and running a partial command is worse than
  a repair turn.
- **Accept only when all parameters of the tool's schema are present.** Rejected:
  the parser does not know the schema. The schema check in `Recovery::accept`
  already enforces the required arguments.

## Consequences

- Easier: 20 of the 24 cut-off replies now produce a call instead of a repair
  turn. The 4 cut mid-argument still fail. No reply that already parsed changed.
- Harder: a cut-off reply can run with an optional argument missing, when the
  model meant to send one more. The risk we accept is narrow: the required
  arguments are checked, and the missing optional one is usually harmless.
- Revisit this if a tool with optional arguments that change behaviour turns out
  to run wrongly after a cut-off.

## Verification

- Parser: `a_call_cut_off_after_its_last_argument_is_kept` and
  `a_call_cut_off_with_no_argument_still_fails`. The existing
  `output_cut_off_inside_an_invoke_is_still_a_failure` covers a cut inside an
  argument.
- End to end through `Dialect::Native.scan` and `Recovery::accept`:
  `a_cut_off_call_runs_only_if_its_arguments_are_complete`.
- Corpus probe, run once and not kept in the repo: zero-call replies went from 40
  to 20, and none of the 55 that parsed before changed. The probe used
  approximate schemas, so re-run it with the real ones.
