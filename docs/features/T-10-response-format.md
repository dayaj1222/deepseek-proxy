# T-10 - `response_format` (`json_object`, `json_schema`)

**Status:** planned · **Effort:** M · **Depends on:** nothing (reuses repair machinery)
**Roadmap entry:** [`PLAN.md`](../../PLAN.md) Phase C

## Problem

A client that sends `response_format: {"type": "json_schema", ...}` gets prose
with the JSON somewhere inside it, a fenced code block, or nothing at all -
and the request succeeds. The proxy is the only place that can notice, because
it is the only place that sees both the request and the whole answer.

## Surface

- `response_format: {"type": "text" | "json_object" | "json_schema"}`.
- `json_schema` carries `name`, `schema`, and `strict`.
- Combination `response_format` + `tools` -> 400 `unsupported_combination`,
  documented: the proxy cannot both demand JSON and allow a tool call.
- A schema with an external `$ref` -> 400, reusing `has_external_ref`.

## Design

**The repair machinery already exists.** A malformed tool call gets a bounded
correction turn; invalid JSON is the same problem with a different prompt. Reuse
`Recovery`, `tool_repair_max_retries` and the `dialect` repair plumbing rather
than writing a second retry loop.

1. Inject a system instruction demanding JSON only, plus the schema when given.
2. Parse and validate the completed answer with `jsonschema`.
3. On failure, run up to `tool_repair_max_retries` repair turns with a
   JSON-specific prompt that quotes the validation error.
4. On final failure, 502 `invalid_model_output`. Never return invalid JSON as a
   success - the caller's `serde_json::from_str` would fail later, further from
   the cause, and the turn would already be billed.

**Streaming is buffered.** `response_format` disables incremental content
delivery: content is accumulated, validated, then emitted as SSE. The response
is still valid SSE (`stream: true` works), just with one content chunk rather
than many. This is documented, because it is a real behavioural difference a
client can observe.

`strict: true` behaves the same as `json_schema` with validation on; the field is
honoured rather than ignored so a caller's expectation is not silently dropped.

## Tests

- valid JSON first time -> returned as-is, no extra turn
- prose then valid JSON on the repair turn -> success, 2 upstream turns
- never valid -> 502 `invalid_model_output`
- schema with external `$ref` -> 400
- `response_format` + `tools` -> 400 `unsupported_combination`
- streaming: one content chunk, body parses as JSON, SSE framing is valid
- `finish_reason: "length"` with incomplete JSON -> 502, not a truncated 200

## Not in scope

Schema composition beyond what `jsonschema` supports, coercion of types (a
string `"5"` is not a number), streaming partial JSON.

## Risks

- **Buffering changes the streaming contract** for this mode. Called out in the
  README and in the error/docs text, and tested, so nobody discovers it in
  production.
- **Repair cost**: each repair is a real billed turn. The bound is the existing
  `tool_repair_max_retries`, which is documented and configurable.
