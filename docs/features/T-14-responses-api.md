# T-14 - `/v1/responses`

**Status:** planned · **Effort:** L · **Depends on:** T-08, T-09, T-10
**Roadmap entry:** [`PLAN.md`](../../PLAN.md) Phase C

## Problem

The Responses API is the surface OpenAI steers new integrations toward, and it is
the one that carries server-side conversation state (`previous_response_id`). A
client written against it cannot use this proxy at all today.

## Design: translate, never duplicate

The plan is explicitly **not** to reimplement the turn. A `ResponsesRequest` is
translated into the existing `ChatRequest`, the shared turn function runs, and the
result is translated back into Responses events/shape. If the translation needs
something `ChatRequest` cannot express, that is an ADR, not a second turn
implementation.

This is the same principle as ADR-0002 (one chunk model, two transports): one
turn, two surfaces.

## Surface

### Request

| Responses field | Mapping |
|---|---|
| `input` (string) | one user message |
| `input` (items) | `message` items map to messages; `function_call_output` maps to a tool result |
| `instructions` | prepended system message |
| `tools` | same function-tool translation as `tools` |
| `previous_response_id` | resolved to a `thread_key` via a new table |
| `store` | when false, do not persist the response id |
| `stream` | `response.output_text.delta` ... `response.completed` |

Unsupported item types -> 400 `unsupported_input_type` naming the type.

### State

New table (migration per PLAN rule 8):

```sql
CREATE TABLE IF NOT EXISTS responses (
  response_id TEXT PRIMARY KEY,
  thread_key  TEXT NOT NULL,
  created_at  INTEGER NOT NULL
);
```

`previous_response_id` looks the thread up and resumes it. An unknown id -> 400
`invalid_previous_response_id`, not a silent new thread: silently starting a new
conversation would lose context without telling anyone.

## Phasing

1. Non-streaming text + function tools + `previous_response_id`. This is the
   useful core and it is all translation.
2. Streaming, event order asserted exactly (`created`, deltas, `completed`).
3. Unsupported item types and `store: false` polish.

## Tests

- non-streaming text round trip
- stream event order is exactly as documented, including `response.completed`
- `previous_response_id` resumes the same thread (assert the thread key and that
  the second turn's delta is only the new message)
- unknown `previous_response_id` -> 400
- tool call round trip: function call, `function_call_output` back, final text
- `store: false` leaves no row
- an unsupported input item type -> 400 naming it

## Not in scope

Built-in tools (`web_search`, `file_search`), `reasoning` items, `include`,
background mode, `input_image` beyond what the chat surface supports.

## Risks

- **Scope creep into a second implementation.** The guard is the rule above: every
  behaviour must be expressible as translation plus the existing turn.
- **Response ids in SQLite** grow without bound. A retention job (or a `--days`
  argument to a future `prune` command) should come with this, not later; note it
  in the commit if deferred.
- **Event-order drift** as OpenAI evolves the API. Assert the order in a test so a
  change is a diff, not a surprise.
