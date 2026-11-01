# ADR-0019: A run of four bare closers ends the upstream read

**Status:** accepted (2026-11-01)
**Supersedes / amends:** none. Sits beside ADR-0011, which is unchanged.

## Context

The model sometimes ends a turn by repeating the bare DSML closer (the closing
marker with no element name), two per line, and does not stop. It keeps going
until its token limit. Every token is billed and wasted, and the client waits on
a stream that carries nothing useful.

ADR-0011 only hides a little of this. `strip_stray_closers` swallows up to two
closers after a completed block; anything past two is emitted verbatim. A real
spam run therefore still reaches the client as visible markup, and still burns
the upstream budget, because the proxy keeps draining the body.

The fix has to stop the generation, not only hide its output. The only stop
signal DeepSeek honours is dropping the proxy-to-DeepSeek connection, which the
client-disconnect path already relies on.

## Decision

`sse::pump` counts the bare closers at the end of the raw text it has received.
When four or more arrive in a row, separated only by whitespace, it leaves the
read loop with the terminal reason `closer spam`. Returning from `pump` drops the
response body and closes the upstream socket.

- The count is `dialect_dsml::trailing_bare_closers`. It reuses `tag_at`, so the
  pipe, whitespace and case rules are the ones the parser already uses.
- Only the bare closer counts. Invoke, parameter and wrapper closers are
  structural and can legitimately repeat in a nested call, so they never cut.
- The threshold is `dialect_dsml::CLOSER_SPAM_THRESHOLD = 4`. It is a constant,
  not a config key.
- The count runs over a 256-byte tail of the text channel (`CLOSER_TAIL_CAP`),
  kept separately from the diagnostic head.
- The cut is not a failure. `Pumped.hint` stays `None`, `stats.errors` is not
  bumped, and everything collected so far is returned unchanged. The call and
  prose before the run go through the normal recovery and cursor path.

## Alternatives considered

- **Raise `MAX_STRAY_CLOSERS` to cover the spam.** Rejected. It saves no tokens
  and puts no bound on how far the model goes. It only hides the markup.
- **Keep draining the body and hide the tags.** Rejected. This is the same token
  burn as today, just quieter.
- **A prompt instruction not to repeat closers.** Rejected. ADR-0014 and
  ADR-0017 forbid teaching the wire format, and the prompt already says "exactly
  once" and is not obeyed.
- **Extend the ADR-0011 swallow instead of cutting.** Rejected for the same
  reason as raising the limit: the generation continues.
- **Threshold of two or three.** Rejected. Two must reach the client so that a
  reply quoting the tag in prose still works (ADR-0011). Three is also the
  deepest legitimate structural close, and a threshold of three would make the
  ADR-0011 third-closer case unreachable.
- **A config key for the threshold.** Rejected. There is no evidence that
  accounts or models need different values, and a knob nobody turns is a cost.
  If that changes, promote the constant in a follow-up.

## Consequences

- Upstream tokens stop being billed as soon as the run is detected, instead of
  until the token limit.
- The client sees a normal end of stream. The run itself is not sent in full:
  the cut happens after the delta that completes the run, so up to about four
  closers may still reach the client as text, as they could before.
- **Accepted risk:** a mid-run cut leaves the remote message partly generated.
  The saved cursor may point at a message the model did not finish. The next
  turn parents to it, and DeepSeek treats it as the previous reply. This is the
  same state the client-disconnect path already produces, so it is not new.
- A call the run truncated is already malformed under ADR-0001, and goes to
  repair exactly as it does today.
- The non-streaming read path (repair turns, `resp.text()`) is not covered. Those
  replies are short by construction, and making them streaming would change the
  repair loop. If spam there matters, it gets its own decision.
- Revisit the threshold if real replies show a legitimate run of four or more
  bare closers, or if spam arrives with fewer than four.

## Verification

- Unit, `dialect_dsml.rs`: `trailing_bare_closers_counts_a_run`,
  `trailing_bare_closers_ignores_structural_closers`,
  `trailing_bare_closers_stops_at_other_bytes`,
  `trailing_bare_closers_tolerates_spelling`,
  `trailing_bare_closers_is_bounded`.
- Streaming, `sse.rs`: `closer_spam_cuts_the_upstream_read`,
  `closer_spam_before_any_call_still_cuts`, `three_closers_do_not_cut`,
  `structural_closers_never_cut`. Each feeds the pump a real `reqwest::Response`
  over chunked bodies. The text after the run sits in a later chunk, so a read
  that does not stop shows up as that text reaching `Pumped.text`.
- Mutation check: with `CLOSER_SPAM_THRESHOLD` set to 1000, the two cut tests
  fail. With the threshold at 4 they pass.
- Not verified here: a live run against a real account
  (`deepseek-rs check`), which is manual per AGENTS.md. The expected evidence is
  `ended="closer spam"` in the `upstream stream finished` debug line.
