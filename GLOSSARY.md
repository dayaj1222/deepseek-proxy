# GLOSSARY

Domain and architecture terms this codebase uses deliberately. Architecture
reviews and refactors reuse these names rather than inventing new ones.

## Architecture terms

- **module** — a named region of code with an interface and an implementation.
  A file, a `mod`, or a section of one.
- **interface** — everything a caller must know to use a module: signatures,
  argument count, error types, ordering guarantees.
- **implementation** — what sits behind the interface. May change freely.
- **depth / deep / shallow** — a module is deep when its interface is small
  relative to its implementation, shallow when the two are nearly the same size.
- **seam** — a place where an implementation can be swapped without editing
  callers. One adapter is a hypothetical seam; two adapters make it real.
- **adapter** — one concrete implementation behind a seam (SQLite, in-memory,
  live HTTP client).
- **leverage** — how much behaviour one interface buys. One seam serving N call
  sites is high leverage.
- **locality** — how close a rule is to the code that must obey it. A rule
  stated once in one module has high locality.

## Domain terms

- **thread** — one conversation with DeepSeek, as seen by the client. Identified
  by `thread_key` (explicit `thread_id`, then `x-thread-id`, then client session
  id, then a message hash).
- **turn** — one client request and everything the proxy does to satisfy it:
  validation, image intake, prompt assembly, the upstream completion, every
  correction turn, and the cursor write. A turn is the unit that holds the
  thread lock.
- **delta** — the messages after the last assistant message. What a continuation
  turn actually sends upstream.
- **replay** — the flattened whole transcript. What a rebuilt session is fed.
- **cursor** — `parent_message_id`: the id of the newest upstream message. Every
  reply must advance it; a stale cursor makes DeepSeek render the next message
  as an edit.
- **repair turn** — a bounded follow-up turn that asks the model to re-emit a
  malformed tool call. Non-streaming, no thinking, no search.
- **malformed** — a tool-call block that parsed but failed schema validation, or
  never closed. Distinct from prose; it routes to repair, never to the client.
- **account** — one DeepSeek login. Threads bind to an account; a login-dead
  account fails the thread over to a healthy one.
- **surface** — the HTTP API (`/v1/chat/completions` etc.). The one place the
  word "interface" is not used.
