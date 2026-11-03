# Feature spec — cut the upstream stream on a closing-tag spam run

**Status:** spec (not implemented)
**Suggested branch:** `feat/closer-spam-cutoff`
**Baseline:** `main` @ `76b5605`
**Area:** `dialect_dsml.rs` (the counter), `sse.rs` (the cut), plus the decision trail

> **Notation.** In this document **bare closer** means the DSML closing tag with
> no element name: open angle bracket, slash, optional fullwidth pipes, the word
> `DSML` in any case, close angle bracket. That is the tag
> `dialect_dsml::tag_at` classifies as `Kind::Bare` with `closing = true`. The
> literal spelling is deliberately not written out here, so this file is not
> itself a wall of markup; the code and the tests use the real string.

---

## 1. Problem

The model sometimes ends a turn by spraying bare closers with nothing between
them, and does not stop by itself. The observed shape is the bare closer
repeated, roughly two per line, for the rest of the reply:

```text
(bare closer) (bare closer)
(bare closer) (bare closer)
(bare closer) (bare closer)
(bare closer) (bare closer)
(bare closer)
(bare closer)
(bare closer)
```

Once it starts it keeps generating the same closer until it hits its token
limit. Every one of those tokens is billed and wasted, and the client waits on a
stream that is producing nothing useful.

Today the proxy only *hides* a little of this: `strip_stray_closers` swallows up
to `MAX_STRAY_CLOSERS` (2) closers after a completed block (ADR-0011), and
anything past two is emitted verbatim — so a real spam run still reaches the
client as visible markup and still burns upstream tokens.

## 2. Goal

Detect the spam run while it is still streaming and **drop the proxy to
DeepSeek connection**, so the model stops generating. Nothing else about the
turn changes.

Non-goals:

- Not a behavioural change to parsing, schema validation, repair, cursors,
  stats, or the client's stream shape.
- Not a new prompt, reminder, or nudge. The model is told nothing.
- Not applied to the non-streaming (repair turn) read path.

## 3. The tag to count

Only the **bare closer** counts. Pipes (fullwidth and ASCII), whitespace and
case inside the marker are ignored, exactly as `dialect_dsml::tag_at` already
does.

Explicitly **not** counted:

| Tag | `tag_at` kind | Counted? |
|---|---|---|
| bare closer | `Bare`, closing | **yes** |
| invoke closer | `Invoke`, closing | no |
| parameter closer | `Parameter`, closing | no |
| wrapper (calls) closer | `Wrapper`, closing | no |

Rationale: the repeated token is the bare closer. The structural closers of a
well-formed block can legitimately appear in a row — a nested call closes with
parameter, then invoke, then wrapper — so counting those would cut real calls.

### 3.1 What counts as a run

A run is **at least `CLOSER_SPAM_THRESHOLD` consecutive bare closers separated
only by whitespace**, with nothing else between them.

- Separators allowed inside a run: space, tab, newline, carriage return. They
  may be split arbitrarily across stream deltas.
- Any other byte ends the run: text, an opening tag, a structural closer, a tool
  call, a code fence.

### 3.2 Threshold

```rust
pub const CLOSER_SPAM_THRESHOLD: usize = 4;
```

Four, not two. ADR-0011 deliberately allows two to reach the client so a reply
that quotes the tag in prose still works; cutting at two would break that. Four
is comfortably above both the two-swallow limit and the deepest legitimate
structural close (three), and still catches the run on its first full
repetition.

The threshold is a constant, not a config key. There is no evidence that
accounts or models need different values, and a config key here would be a knob
nobody turns. If that proves wrong, promote it in a follow-up — the rule is one
constant.

## 4. Where the run is detected

### 4.1 Why not in the existing filter

`strip_stray_closers` runs **after** a completed block has been consumed, on
what is left in `Parser::pending`. A spam run that arrives before any block, or
between blocks, never passes through it. And by the time it does run, the first
two closers have already been dropped, so a "four in a row" test there would be
testing the wrong bytes.

The counter must see the raw text, not the post-strip text.

### 4.2 The one implementation

One helper, in `dialect_dsml.rs` (the module that already owns DSML spelling):

```rust
/// How many bare closers, separated only by whitespace, are still visible at
/// the end of `text`. Returns 0 when the text does not end inside or
/// immediately after such a run.
pub fn trailing_bare_closers(text: &str) -> usize
```

It walks backwards from the end of `text`, skipping ASCII whitespace, matching
the bare closer with the same pipe/whitespace/case normalization as `tag_at`,
and counts matches. It returns 0 as soon as it hits a byte that is neither
whitespace nor the tail of a closer.

A run only ever appears at the **end** of what has arrived so far, so a trailing
counter is sufficient and carries no state between deltas. If the run is broken
by an intervening byte the count resets naturally. The implementation must be
O(bytes of the run), never O(len(text)) per delta; the caller keeps a bounded
buffer so the helper only ever sees the last few hundred bytes.

### 4.3 What the pump passes it

`sse::pump` keeps a bounded `head` of the *first* 1024 bytes for diagnostics.
That buffer is not reusable: the run is at the end. Add a second, small,
tail-capped buffer of the last `CLOSER_TAIL_CAP` bytes of text.

On every text event, after `filter.feed(&ev.text)`:

1. Append `ev.text` to the tail buffer; trim from the front down to the cap, at
   a `char` boundary so the buffer stays valid UTF-8.
2. If `dialect_dsml::trailing_bare_closers(&tail) >= CLOSER_SPAM_THRESHOLD`,
   break out of the read loop with the new terminal reason.

## 5. What the cut does

Break out of the read loop with a new terminal reason:

```rust
ended = "closer spam";
```

Then exit exactly the way `"upstream closed"` already exits:

1. `finish("closer spam", raw_bytes, raw_events)` runs — the existing debug
   summary line, one line, new reason.
2. `resp` / `byte_stream` are dropped when `pump` returns, which closes the
   proxy-to-DeepSeek socket. **That is the stop signal.** There is no stop
   endpoint; dropping the connection is the only mechanism DeepSeek honours, and
   it is the same one the client-disconnect path already relies on.
3. `Pumped.hint` stays `None`. The turn is **not** a failure: it does not route
   to `fail_turn`, does not bump `stats.errors`, and raises no hint error.
4. Everything already collected is returned unchanged: `parent` (the cursor),
   `filter` (calls and malformed), `emitted`, `text`, `prose`, `prompt_tokens`.

The client connection is **not** touched. It sees a normal end of stream.

## 6. Downstream is unchanged — the important part

After `pump` returns, `stream_response` runs exactly as it does today
(`main.rs` around lines 1310 to 1900):

- The cursor advances on the reply that was received (the normal success path;
  the stream did not error).
- `filter.calls` and `filter.malformed` go through `Recovery` unchanged: schema
  validation, `require_call` for `tool_choice: "required"`, the repair loop, the
  throttled-repair backoff, `end_repair`, `declined_repair`.
- A call that the run truncated is **already** malformed under the existing
  rules (`Parser::finish` returns `unterminated_block`, ADR-0001) and routes to
  repair exactly as today.
- `finish_reason` is `tool_calls` when calls exist, `stop` otherwise — unchanged.
- The rate ticket (`slot`) is dropped after `pump` returns, so `request_delay`
  is measured from the cut, not from a stream that kept running.

**No other file's control flow changes.** `main.rs` needs no new branch.

## 7. Non-streaming path: out of scope

Repair turns read with `resp.text()` (`main.rs` around line 1735), which buffers
the whole body before any parsing. Making those stop early would mean converting
them to a streaming read — a behavioural change to the repair loop for a case
that is short by construction (a repair reply is a correction, not a long
generation).

They inherit only ADR-0011's two-tag swallow today, and that is unchanged. If
spam in repair replies turns out to matter it gets its own spec, because it
touches the repair loop and the `slot`/`hint` handling around it.

## 8. Interaction with ADR-0011 — no change

ADR-0011's two-tag swallow stays exactly as it is. The rules are independent:

| Closers in a row | Behaviour |
|---|---|
| 1 to 2 | swallowed by `strip_stray_closers` (ADR-0011), never sent |
| 3 | two swallowed, the third emitted verbatim (ADR-0011) |
| 4 or more | connection cut before anything past the run is read |

The threshold of four is what makes the third row unreachable for a real spam
run: the model does not stop at three.

## 9. Tests

Unit, `dialect_dsml.rs`:

- `trailing_bare_closers_counts_a_run` — the section 1 text counts at least 4.
- `trailing_bare_closers_ignores_structural_closers` — repeated invoke,
  parameter and wrapper closers count 0.
- `trailing_bare_closers_stops_at_other_bytes` — a closer, a character, a closer
  counts 1, not 2.
- `trailing_bare_closers_tolerates_spelling` — lowercase, extra pipes, newlines
  and tabs between tags all count.
- `trailing_bare_closers_is_bounded` — 10 KB of prose ending in 4 closers counts
  4.

Streaming, `sse.rs` (a `Chunk`-level test on the pump's decision, or the mock
upstream if one is available):

- `closer_spam_cuts_the_upstream_read` — one valid tool call, then the section 1
  run: the read stops early, `Pumped.hint` is `None`, the call is in
  `filter.calls`, the summary reason is `closer spam`.
- `closer_spam_before_any_call_still_cuts` — prose then spam: `filter.calls`
  empty, turn ends `stop`, no error.
- `three_closers_do_not_cut` — three bare closers are left to ADR-0011.
- `structural_closers_never_cut` — a nested call closing parameter, invoke,
  wrapper is not cut.

Regression: `cargo test --workspace` stays green (263 at baseline).

## 10. Verification

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets   # no new warnings vs baseline 28
```

Manual, against a real account (`deepseek-rs check` — never in an automated
test, per AGENTS.md): a turn whose reply ends in the section 1 run logs
`ended="closer spam"` in the `upstream stream finished` debug line, and the
client receives the tool call or prose that preceded the run.

## 11. Decision trail the change owes

- **CHANGELOG entry** — reason: the model burns its whole token budget spraying
  closers, and hiding the extra tags after the fact does not stop the burn;
  cutting the connection does.
- **ADR-0019 — "a closing-tag spam run ends the upstream read"** — because this
  is a real alternative to ADR-0011's swallow (extend the swallow instead of
  cutting) and it accepts a risk. Record:
  - rejected: raise `MAX_STRAY_CLOSERS` to cover the spam — does not save a
    single token and puts no bound on how far the model will go;
  - rejected: keep draining the body and only hide the tags — same token burn;
  - rejected: a prompt instruction not to repeat closers — ADR-0014 and ADR-0017
    forbid teaching the wire format, and the prompt already says "exactly once"
    and is not obeyed;
  - accepted risk: a mid-run cut leaves the remote message partially generated,
    so the saved cursor may point at a message the model did not finish. The
    next turn parents to it and DeepSeek treats it as the previous reply — the
    same state the client-disconnect path already produces.
- **GLOSSARY** — add **closer spam**: a run of bare DSML closers with nothing
  between them, which ends the upstream read instead of being parsed.

## 12. Constants to add

| Name | Where | Value | Meaning |
|---|---|---|---|
| `CLOSER_SPAM_THRESHOLD` | `dialect_dsml.rs` | 4 | bare closers in a run that trigger the cut |
| `CLOSER_TAIL_CAP` | `sse.rs` | 256 | bytes of trailing text inspected per delta |

Both private to their module. Neither is a config key.

## 13. Files touched

| File | Change |
|---|---|
| `crates/proxy/src/dialect_dsml.rs` | `trailing_bare_closers`, `CLOSER_SPAM_THRESHOLD`, unit tests |
| `crates/proxy/src/sse.rs` | tail buffer, cut branch, `closer spam` reason, `CLOSER_TAIL_CAP`, tests |
| `docs/adr/0019-...md` | new ADR |
| `docs/README.md` | ADR index row 0019 |
| `docs/CHANGELOG.md` | entry |
| `GLOSSARY.md` | **closer spam** |

`main.rs`, `dialect.rs`, `dialect_simple.rs`, `pool.rs`, `store.rs` are **not**
touched.

## 14. Open question for the implementer

The threshold and the tag set are fixed above. One thing to check while writing
the counter: `normalize_dsml` rewrites marker spellings before parsing, but the
tail buffer sees raw upstream text. Either normalize the tail buffer the same
way, or make `trailing_bare_closers` do the pipe/whitespace/case folding itself.
The second is cheaper and keeps the helper self-contained — prefer it.

