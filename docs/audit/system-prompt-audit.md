# System-prompt audit

What the model actually receives, block by block, and what should change. Every
claim is read from the source. Sizes are approximate character counts with a
rough token estimate (chars / 4).

**Measured, not estimated:** [`rendered/`](rendered/) holds the exact bytes the
model receives, dumped from the real functions by a throwaway test that was then
removed:

| File | What | Bytes |
|---|---|---|
| [`rendered/01-tool-protocol-plus-schemas.txt`](rendered/01-tool-protocol-plus-schemas.txt) | `inject_tool_descriptions("", tools)` with one small tool | 2131 |
| [`rendered/02-periodic-reminder.txt`](rendered/02-periodic-reminder.txt) | `periodic_reminder()` | 677 |

So the fixed protocol block plus one trivial tool is **2131 bytes (~530 tokens)**,
and the per-turn tail reminder is **677 bytes (~170 tokens)**. A realistic
client with 10-30 tools puts the schema section in the tens of kilobytes.

## 1. How the prompt is assembled

```
turn.rs:52      system_extra = dialect::inject_tool_descriptions("", &req.tools)
turn.rs:84      build_prompts(req, existing, &system_extra, image_note)
main.rs:309     build_prompts -> prompt::for_turn(Turn { ... })
prompt.rs:51    for_turn: transcript -> +system_extra -> +image_note -> +reminder -> +directive
```

Two consequences are easy to miss:

- **The tool protocol is not a system message.** `inject_tool_descriptions` is
  called with an empty base (`turn.rs:52`) and the result is appended to the
  flattened transcript (`prompt.rs:56`), so it lands inside the user prompt. The
  client's own system message is a line in that transcript, not a system role.
  There is no system/instruction separation at all.
- **The upstream has no tool parameter.** `CompletionBody`
  (`crates/client/src/lib.rs:141`) carries `prompt: String` and no `tools` field,
  so the schemas must be rendered as text. There is no transport-level duplicate
  to remove. The duplication is entirely inside our own prompt text (section 3).

## 2. Every block the model sees, in order

Order for a fresh thread (`prompt.rs:51-93`). Sizes are approximate.

| # | Block | Source | Size |
|---|---|---|---|
| 1 | `## TOOL CALL PROTOCOL - MANDATORY, STRICT FORMAT` | `dialect.rs:71` | ~50 chars |
| 2 | "To call a tool, emit a block in EXACTLY this format" + `TEMPLATE` | `instruction()`, `dialect.rs:81-87` | ~330 chars |
| 3 | `### Required tool-call rules`, seven numbered rules | `instruction()`, `dialect.rs:88-106` | ~1070 chars |
| 4 | `## Available Tools` | `dialect.rs:72` | ~25 chars |
| 5 | One `### <name>` section per tool: description, prose note, pretty-printed JSON Schema | `tool_section`, `dialect.rs:74-79` | client-dependent; usually the largest block |
| 6 | `FINAL REMINDER` + `TEMPLATE` + two lines | `final_reminder()`, `dialect.rs:110-118` | ~340 chars |
| 7 | `HARD FORMAT REQUIREMENT` + `TEMPLATE` + `!!IMPORTANT!!` + two lines | `periodic_reminder()`, `dialect.rs:126-143` | ~700 chars |
| 8 | delta / continuation prompt | `main.rs:658` | - |
| 9 | image note, only when images were dropped | `prompt.rs:67` | ~80 chars |
| 10 | `periodic_reminder()` again, at the tail | `prompt.rs:81-84` | ~700 chars |
| 11 | `tool_choice` directive | `prompt.rs:100-122` | ~40 chars |
| 12 | separate turn: repair prompt + `periodic_reminder()` again | `dialect.rs:508-545` | ~1200 chars |

Fixed overhead on a first turn is **~2.5k chars (~630 tokens)** before any tool
schema. Every later turn carries **~700 chars (~175 tokens)** of tail reminder
alone.

## 3. Redundancy, ranked by cost

### 3.1 TEMPLATE is stated four times on turn one

The literal template line (dialect.rs:68-69) appears in instruction() (block 2),
final_reminder() (block 6), and both copies of periodic_reminder() (blocks 7 and
10). A fresh thread prepends the reminder and appends it (prompt.rs:76-84), and a
test asserts exactly that: reminder_is_appended_to_every_turn checks starts_with
and ends_with. A repair turn adds a fifth copy (dialect.rs:542).

Fix: one canonical statement of the shape, one short restatement at the tail.
Drop the head reminder, because block 2 is already at the head. Saves ~700 chars
on turn one and removes ~250 chars of duplicated prose overall.

### 3.2 Rule 7 restates block 5

Rule 7 says to use only parameters from the schema below. Block 5 is that schema,
printed in full with required and additionalProperties. The rule adds nothing a
reader of JSON Schema did not already have.

### 3.3 Rule 1 restates the header and the example

Rule 1 requires a name attribute on the opening tag and notes that extra
attributes are ignored. Block 2 already shows the tag and the sentence after it
says where the name goes. Extra attributes being ignored is a parser fact, not an
instruction: nothing in extract_tool_calls rewards knowing it.
### 3.4 Rule 4 is client policy, hardcoded in the server

Rule 4 permits several blocks per turn only for short, independent, read-only
calls (reading files, listing directories, grepping, small quick commands) and
forbids batching writes, edits and long-running commands.

This is pi's batching policy. Grepping, edits and long-running commands are not
concepts this proxy has. It is the only rule that is a policy decision rather
than a wire-format fact, and the one most likely to differ per client. As
written, every client inherits pi's read-only batching preference.

Fix: keep the wire-format part (several complete blocks are allowed) here; move
the batching policy to the caller's system prompt, where it can change without a
proxy release.

### 3.5 final_reminder contradicts rule 4

final_reminder says to emit each tool call exactly once and never repeat an
opening tag, closing tag or the complete block. Read literally, that discourages
the multiple-blocks rule 4 permits. It is meant as anti-echo after a repair turn
(build_repair_prompt: do not quote, copy, explain or repeat it), and that is where
it belongs, not in every turn.

## 4. Correctness bugs found while auditing

### 4.1 Rule 3 is not implemented anywhere (highest severity)

Rule 3 tells the model: if a value must contain a literal opening or closing
tag, write a backslash directly before it.

Nothing honours this. Every scanner matches the raw substring and never looks at
a preceding backslash: extract_tool_calls (dialect.rs:192), scan (dialect.rs:312),
the streaming Parser (dialect.rs:437, 670, 1300), and strip_stray_closers.
Searching dialect.rs for unescape or backslash returns one hit: the line that
tells the model to escape (dialect.rs:96). No consumer exists.

So a model that follows rule 3 emits a backslash before the closing tag, the
parser closes the block at that position, and the arguments are truncated. The
prompt instructs a workaround that does not work and actively breaks the parse.

Three ways out:

1. Implement it: in all three scanners, treat a closing tag preceded by a
   backslash as literal, and strip the backslash from the body before parsing.
2. Delete the rule: accept that a literal closing tag inside a value is
   unrepresentable. Cheapest, but then the prompt must not invite it.
3. Use JSON's own escape: tell the model to write \u003c for a literal less-than
   sign, which serde_json already decodes for free. No scanner needs to know
   about it at all.

Option 3 fits this codebase best: the block body is JSON, and JSON already has
the escape.

### 4.2 The reminder tells the model the opposite of what the code does

The periodic reminder says violations are NOT repaired, that a malformed block is
DROPPED and never executed, and that the model will burn the turn on a
correction loop instead.

That contradicts itself in one line (not repaired ... correction loop), and the
second half is the true one: malformed calls go to build_repair_prompt for up to
tool_repair_max_retries turns. A model told a false consequence has an incentive
to guess rather than repair.

Fix: state what happens - a malformed call is repaired in a correction turn,
which costs an extra round trip.

### 4.3 The DSML death threat

The reminder's first numbered point forbids the DSML-prefixed spellings of the
tags and ends with a threat that the model will be killed.

That is hostile, ungrammatical and factually loose: normalize_dsml
(dialect.rs:244) converts those tags into the correct form, so they are neither
useless nor shown to the user. Either drop the rule (the normaliser handles it)
or state it plainly: emit the plain tag, not the DSML spelling.

### 4.4 Reminder gating uses a different list than the directive

build_prompts (main.rs:316) decides whether to show the reminder from req.tools,
but the directive comes from the filtered specs (main.rs:324). With
tool_choice: none, or an all-unsupported tool list, the model gets the full
protocol and schemas plus "Do not call tools in this turn". Harmless but
inconsistent: gate both on specs.

## 5. Proposed tightened prompt

One statement of the shape, one statement of the failure modes, no policy, no
false claims. Roughly 40 percent smaller than today fixed overhead, and more
importantly true.

```
## Tool calls

Emit one block per call, in the same shape as the template shown at the top
(dialect::TEMPLATE):

- the name attribute holds the tool name;
- the body is one JSON object holding every argument;
- several complete blocks are allowed in one reply;
- no code fences, no bare JSON;
- inside a JSON string write \u003c for a literal less-than sign, so a tag
  written inside a value cannot end the block.

## Tools

### <tool name>
<description>
<JSON Schema>

## If you get the format wrong

The call is repaired in a correction turn: one extra round trip, nothing is
lost. Re-emit only the failing call, once, with no explanation.
```

What this drops, and why:

| Dropped | Reason |
|---|---|
| Rule 1 (name attribute) | The example in block 2 already shows it |
| Rule 3 as written | Replaced by JSON unicode escape, which needs no scanner support |
| Rule 4 batching policy | Client policy; belongs in the caller system prompt |
| Rule 5 (you may write text) | Default behaviour; nobody assumes the opposite |
| Rule 6 (call only when needed) | Default behaviour; also duplicates the tool_choice directive |
| Rule 7 (no guessing) | The schema below already says it |
| final_reminder | Its anti-echo content moves to the repair prompt |
| Head reminder | Block 2 is already at the head |
| DSML death-threat line | normalize_dsml already handles DSML |
| NOT-repaired / DROPPED | False; replaced with what really happens |

## 6. Does precision help the model?

Yes, but not the kind this prompt reaches for. The evidence here points one way:

- Precision about consequences helps. A malformed call costs one repair turn is
  actionable; a death threat is noise. Rule 4 distinction between read-only
  batching and stateful serialisation is real information, just in the wrong
  layer.
- Precision about the wire format helps most when it is true. Rule 3 is the most
  precisely worded rule in the prompt and the one that does not work: a model
  that follows it produces a worse result than one that ignores it.
- Repetition does not add precision. Four copies of one example teach nothing the
  first did, and they crowd out the tool schemas, the only part of the prompt
  carrying information the model cannot infer.
- A false statement costs more than a missing one. The model calibrates on the
  prompt; if dropped-and-never-executed is false, its model of the system is
  wrong and the rest is discounted.

The direction is fewer, true sentences. The largest single win is not length, it
is fixing section 4.1.

## 7. Suggested order of work

1. Fix or delete rule 3 (4.1). Correctness; do this alone, with a test that a body
   containing a literal tag survives a round trip.
2. Tell the truth about repair (4.2) and drop the DSML line (4.3). Pure prompt
   edit.
3. Deduplicate the template and drop the head reminder (3.1), rewriting
   reminder_is_appended_to_every_turn to assert one occurrence.
4. Move batching policy out (3.4), which needs an ADR on whether the proxy accepts
   a client-supplied policy hint at all.
5. Gate reminder and directive on the same list (4.4).

Steps 1-3 are independent and each is one commit; step 4 needs an ADR.

