# T-23 - json_invoke vs simple_tool_tag

**Status:** implemented | **Effort:** S | **Depends on:** nothing
**Code:** `crates/proxy/src/dialect.rs`, `dialect_simple.rs`, `prompt.rs`, `sse.rs`, `turn.rs`

## Why this document exists

Two wire dialects are live and selectable at runtime (`[proxy] dialect`, or the
`DIALECT` env var). `Dialect::parse` hard-errors on an unknown name rather than
falling back. They are **not** two spellings of the same protocol: they differ in
grammar, in tolerance, in what the parsed call contains, and therefore in which
failures are possible.

`JsonInvoke` is the default and the original behaviour; it must stay
byte-identical. `SimpleToolTag` trades a smaller teaching surface for a parser
that does its own delimiting.

## The two shapes

`JsonInvoke`, where OPEN/CLOSE stand for the invoke tags:

    OPEN name="bash">{"command":"ls"}CLOSE

`SimpleToolTag`, where MARKER is the bare tool marker:

    MARKER{"name":"bash","arguments":{"command":"ls"}}CLOSE

In `JsonInvoke` the name is an attribute of the opening tag and the body is the
argument object. In `SimpleToolTag` the marker is a bare separator; the name and
the arguments both live inside one JSON object, and a closing tag terminates the
call.

## Differences

### 1. Call grammar

`json_invoke` pairs an opening tag carrying the name with a closing tag.
`simple_tool_tag` reads the JSON object after the marker with a real JSON parser,
so the object itself delimits the call:

- the closing tag is optional, and a `</tool>` inside a string value is harmless;
- text after the call (a sentence, a code fence) is prose, not part of the call;
- every block in a reply is its own call, and a broken block does not poison the
  next one;
- a marker not followed by `{` is plain prose, so mentioning the tag does not
  swallow the rest of the reply.

The streaming filter emits prose up to the first marker, holds the rest, and
parses it at end of stream; prose after the calls is returned by `finish`.

### 2. What ToolCall.arguments holds

An object is stored re-serialised (compact, keys sorted); a double-encoded string
is accepted only if it holds an object and is stored as the model wrote it.
`json_invoke` stores the raw body. History replay of `simple_tool_tag` inserts
the stored arguments verbatim, so replay does not drift from what was parsed.

### 3. Failure reasons

Failures use the `json_invoke` vocabulary so `repair_schemas` can re-send the
failing tool's schema: missing or non-object arguments are `body_not_object`,
broken JSON is `invalid_json_body`. For broken JSON the tool name is salvaged
from the text when a `"name"` appears before `"arguments"`. A body with no
usable name is `invalid_arguments_json` and gets no schema.

### 4. DSML recovery

The backend drifts into the DSML spelling on its own (`dialect_dsml.rs`). A
region that parses becomes ordinary calls. One that does not becomes a malformed
call with reason `dsml_unparsable:<detail>`, and the repair prompt then states
what the model did, why it could not run, and not to use DSML again. Plain
`<invoke>` is not recovered; it stays text.

### 5. Tool results

Both dialects render `Tool result (id=<call id>):` followed by the content, so a
result is attributed by id and needs no escaping.

### 6. Teaching surface

`json_invoke` injects a protocol header, seven numbered rules, pretty-printed
JSON Schemas and a final reminder. `simple_tool_tag` injects one paragraph, four
short rules (no code fence, schema parameters only, do not repeat a call after
its result, answer directly when no tool fits) and one raw function object per
tool.

## Choosing

| | json_invoke | simple_tool_tag |
|---|---|---|
| Calls per reply | many | many |
| Prose around calls | kept | kept |
| Trailing text | tolerated | tolerated |
| DSML drift | normalised, params dropped | parsed, or explained to the model |
| Result attribution | by id | by id |
| Argument fidelity | verbatim | object re-serialised, replay verbatim |
| Teaching size | large | small |

`simple_tool_tag` is the cheaper prompt; `json_invoke` still has the longer
rule list and the legacy normaliser.

## Operational note

The dialect is snapshotted per turn (`turn::prepare` into `Prepared.dialect`) and
threaded through `pump` and `Recovery`. A config reload flips it mid-thread
(`changed.push("dialect")`) while the remote session still holds history in the
previous shape. There is no migration: switching dialects on a live thread mixes
formats in one conversation.
