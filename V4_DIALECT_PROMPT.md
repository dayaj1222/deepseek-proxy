# Migrate deepseek-rs proxy: add DeepSeek V4 `｜DSML｜` tool dialect

## Goal

Add a **second** wire dialect to the proxy that renders and parses DeepSeek V4's
native tool-call format, selectable at runtime. Do **not** delete the existing
`json_invoke` path — it works (`.run-serve.log` shows `repair=0 malformed=0`) and
we need to A/B the two.

## Source of truth

`ggml-org/llama.cpp` -> `models/templates/deepseek-ai-DeepSeek-V4.jinja`
(fetched 2026-10-25). Reproduce its rendering **exactly**. Do not invent variants,
do not "improve" the spacing.

## Critical literals

- `｜` is U+FF5C FULLWIDTH VERTICAL LINE. Never substitute ASCII `|`.
- `▁` is U+2581 LOWER ONE EIGHTH BLOCK. Never substitute `_` or a space.
- The DSML sentinel is the 6 characters: `｜DSML｜`.
- BOS is `<｜begin▁of▁sentence｜>`, EOS is `<｜end▁of▁sentence｜>`.
- Role markers are `<｜User｜>` and `<｜Assistant｜>`.

## Where the current code lives

- `crates/proxy/src/dialect.rs` — teaching text, `ToolCall { name, arguments }`,
  `fingerprint()`, `INVOKE_OPEN`/`INVOKE_CLOSE`, `periodic_reminder()`,
  `inject_tool_descriptions()`.
- `crates/proxy/src/prompt.rs` — `Prompt { delta, replay }`, `for_turn()`,
  ordering rules, `directive()`, `reminder()`.
- `crates/proxy/src/main.rs` — `render_message()`, `flatten_transcript()`,
  `delta_messages()`, `continuation_prompt()`.
- `crates/proxy/src/turn.rs` — request/response loop.

Refactor: introduce `enum Dialect { JsonInvoke, DeepSeekV4Dsml }` with trait-ish
methods `render_tool_call(&ToolCall) -> String`, `parse_tool_calls(&str) ->
(String /* prose */, Vec<ToolCall>)`, `system_extra(&[Value] /* tool
schemas */) -> String`, `reminder() -> String`, `render_tool_result(name,
content) -> String`. Keep all existing `json_invoke` behaviour byte-identical.
Wire the old free functions to delegate to `Dialect::JsonInvoke`.

## Outbound rendering (client -> DeepSeek) for `DeepSeekV4Dsml`

### 1. System prompt

Emit `BOS`, then the concatenation of every `system` message (joined by
`\n\n`), then — **if tools were offered** — append `\n\n` + tools_header +
schemas + tools_footer.

tools_header (exact, note the blank lines):

```
## Tools

You have access to a set of tools to help answer the user's question. You can invoke tools by writing a "<｜DSML｜tool_calls>" block like the following:

<｜DSML｜tool_calls>
<｜DSML｜invoke name="$TOOL_NAME">
<｜DSML｜parameter name="$PARAMETER_NAME" string="true|false">$PARAMETER_VALUE</｜DSML｜parameter>
...
</｜DSML｜invoke>
<｜DSML｜invoke name="$TOOL_NAME2">
...
</｜DSML｜invoke>
</｜DSML｜tool_calls>

String parameters should be specified as is and set `string="true"`. For all other types (numbers, booleans, arrays, objects), pass the value in JSON format and set `string="false"`.

If thinking_mode is enabled (triggered by <think>), you MUST output your complete reasoning inside <think>...</think> BEFORE any tool calls or final response.

Otherwise, output directly after </think> with tool calls or final response.

### Available Tool Schemas

```

Then one line per tool: the tool's `function` object serialised as JSON
(`{"name":...,"description":...,"parameters":...}`), each terminated by `\n`.

tools_footer (exact): `\nYou MUST strictly follow the above defined tool name
and parameter schemas to invoke tool calls.\n`

### 2. User and developer messages

Each becomes `<｜User｜>` + content. Consecutive user/developer/tool messages
collapse into **one** `<｜User｜>` turn, joined with `\n\n`.

### 3. Tool results

**Not** `tool▁output▁begin` (that was V3.1). V4 folds tool results into the
current user turn as:

`<tool_result>` + content + `</tool_result>`

If the preceding message was already a user/tool message, prefix `\n\n`
instead of opening a new `<｜User｜>`.

### 4. Assistant messages

`<｜Assistant｜>`, then a thinking region, then content, then optional tool
calls, then `<｜end▁of▁sentence｜>`.

Thinking region: if reasoning is being kept, `<think>` + reasoning_content +
`</think>`; otherwise just `</think>`.

Tool-call block when `tool_calls` is non-empty — prefix `\n\n` then:

```
<｜DSML｜tool_calls>
<｜DSML｜invoke name="NAME">
<｜DSML｜parameter name="KEY" string="true">VALUE</｜DSML｜parameter>
<｜DSML｜parameter name="KEY2" string="false">JSON</｜DSML｜parameter>
</｜DSML｜invoke>
</｜DSML｜tool_calls>
```

Rules: for each argument, if the JSON value is a string emit
`string="true"` and the value **raw, unescaped**; otherwise emit
`string="false"` and the value re-serialised as JSON. One parameter per line,
newline-terminated. Close each invoke before the next. If an invoke has no
arguments, emit a bare newline between open and close tags.

### 5. Generation prompt

`<｜Assistant｜>` followed by `<think>` when thinking is on, else `</think>`.

## Inbound parsing (DeepSeek -> client) for `DeepSeekV4Dsml`

Scan the assistant text for `<｜DSML｜tool_calls>` .. `</｜DSML｜tool_calls>`.
For each `<｜DSML｜invoke name="X">` .. `</｜DSML｜invoke>`, collect its
`<｜DSML｜parameter name="K" string="true|false">V..</｜DSML｜parameter>` children
and build an arguments object: `string="true"` means use V verbatim as a
string; `string="false"` means parse V as JSON.

Produce `ToolCall { name: X, arguments: serde_json::to_string(&obj) }`.
Return the prose with the whole `tool_calls` block removed.

Tolerances (models drift):
- bare `<｜DSML｜invoke>` with no wrapping `tool_calls` block
- repeated closing tags (reuse the existing `MAX_STRAY_CLOSERS` idea)
- a parameter whose `string=` attribute is missing: infer from the value
  (starts with `{`, `[`, a digit, `true`, `false`, `null` -> JSON; else string)
- surrounding whitespace and `\r\n`
- `arguments` that fail to parse: keep the raw text so the repair path can
  report `invalid_arguments_json` exactly as it does today

## Config

Add to `[proxy]` in `config.toml`, defaulting to the current behaviour:

```toml
[proxy]
dialect = "json_invoke"   # or "deepseek_v4_dsml"
```

Env override `DIALECT=deepseek_v4_dsml`. Unknown value = hard error at startup,
not a silent fallback.

## What must NOT change

1. `json_invoke` output byte-identical when `dialect = "json_invoke"`.
2. The repair machinery: `tool_repair_max_retries`, the correction turn, the
   `invalid_arguments_json` reporting, echo-detection via `fingerprint()`.
3. Per-turn re-anchoring: `periodic_reminder()` still appended at the tail of
   every turn. Write a DSML-specific reminder text for the new dialect (it must
   restate the invoke/parameter shape and the two failure modes we actually see:
   doubled closing tags and a missing `string=` attribute).
4. Account pool, PoW, `parent_message_id` threading, continuation logic,
   image handling, SSE keep-alive.
5. `flatten_transcript()` / `delta_messages()` semantics — only the *rendering*
   of each message changes, not which messages are selected.

## Tests to add

Unit tests in `crates/proxy/src/dialect.rs` (or a new `dialect/tests.rs`):

1. Round-trip: build `ToolCall { name: "read", arguments: "{\"path\":\"/tmp/x\"}" }`,
   render it, parse it back, assert name and arguments match.
2. String vs non-string parameters: render `{path: "/a", count: 5, deep: true}`
   and assert `path` gets `string="true"` with a raw value while `count` and
   `deep` get `string="false"` with JSON.
3. Missing `string=` attribute is inferred correctly.
4. Two invokes in one wrapper parse into two `ToolCall`s in order.
5. Bare invoke with no wrapper parses.
6. A DSML block embedded in prose is removed from the returned prose.
7. Malformed JSON in a non-string parameter survives as raw text (repair path).
8. Golden test: one full three-message conversation (system with one tool, user,
   assistant with one tool call, tool result, user) renders to an exact expected
   string. Store the expectation in a fixture file so a whitespace regression
   fails loudly.
9. `dialect = "json_invoke"` golden test unchanged from `main` — prove no
   regression.

## Acceptance

```
cargo test -p proxy
cargo clippy -p proxy -- -D warnings
```

Then, with `DIALECT=deepseek_v4_dsml cargo run -- serve`:

1. A single-turn prompt with one tool returns a well-formed `tool_calls` array.
2. A two-turn prompt (tool call then tool result) continues correctly and the
   second reply references the tool output.
3. `.run-serve.log` shows no `malformed` lines for 20 consecutive tool calls.
4. Switch back to `DIALECT=json_invoke` and confirm identical behaviour to
   today.

## Reporting back

Report: which files changed, the exact diff of `dialect.rs`'s public surface,
the golden fixture contents, and the results of the 4 acceptance runs. If any
test fails, stop and report rather than loosening the assertion.
