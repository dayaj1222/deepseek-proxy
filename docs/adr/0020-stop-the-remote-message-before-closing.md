# ADR-0020: The client stops the remote message before it closes the connection

**Status:** accepted (2026-11-01)
**Supersedes / amends:** amends ADR-0019. The cut and its threshold stand; its
claim that dropping the connection is the only stop signal is wrong.

## Context

ADR-0019 cut the upstream read on a run of bare closers and assumed that
dropping the connection would stop DeepSeek's generation. The log after the
merge showed the opposite. Every turn the proxy cut came back with
`ended="closer spam"`, and the next request that parented to that message was
rejected with `{"biz_code":11,"biz_msg":"message still wip"}`. A minute's wait
let the message finish, so one request went through and then failed again.

A HAR recorded from the DeepSeek web client answered the question. Pressing
stop there sends `POST /api/v0/chat/stop_stream` with
`{"chat_session_id", "message_id"}`, where `message_id` is the
`response_message_id` of the stream. The response is `biz_code 0`. The same HAR
then shows a completion with `parent_message_id` set to the stopped message
being accepted. Stopping during thinking and during the answer both use this
request.

The stop call needs the same authorization and cookies as the other API calls.
It needs no proof-of-work challenge and no `x-hif-*` headers.

## Decision

The stop lives in the `client` crate, not in the proxy.

- `Client::stop_stream(session_id, message_id)` posts the stop request.
- `stream_completion` returns a `client::Completion`, a stream of the response
  bytes. It reads `response_message_id` from the bytes it yields (first one
  wins), so callers never pass a message id.
- `Completion::stop()` sends the stop while the connection is still open, then
  closes the connection. It is bounded by a 5 second timeout.
- Dropping a `Completion` before its stream ends sends the same stop in the
  background. This covers a client that hangs up, a request timeout, and an
  error in the middle of the reply.
- `Completion::text()` reads a whole body, for the non-streaming repair path.

The proxy's closer-spam cut calls `byte_stream.stop()` before it breaks out of
the read loop. Its threshold and tail logic are unchanged (ADR-0019).

## Alternatives considered

- **Proxy-level stop.** Hand the account's client and session id to `pump`
  through the account pool. This was implemented and then rejected. Every caller
  of the completion would have to carry the stop plumbing, and the next caller
  to forget it would recreate the `wip` bug.
- **Dropping the connection alone.** This is the current behaviour, and the log
  shows it leaves the message `wip`.
- **Waiting for the model to finish.** This brings back the token burn that the
  cut exists to stop.
- **A prompt instruction to stop repeating closers.** Rejected by ADR-0014 and
  ADR-0017, and the prompt is not obeyed in any case.

## Consequences

- A client hang-up, a timeout, or an error mid-reply now stops the remote
  message too. Before, each of those left the message `wip`. This is a change of
  behaviour for every caller of the client crate, not only the closer-spam cut.
- The stop call is best effort. If it fails or times out, the connection is
  closed anyway and a warning is logged. The next turn on that message can then
  still fail, as it did before this change.
- On the explicit path (`stop()`), the stop is sent before the connection
  closes, which matches the web client. On the drop path the stop is spawned,
  so it is sent after the connection has been dropped. The web client's exact
  ordering is not in the HAR, so the drop path's ordering is not verified.
- **Accepted risk:** the stop depends on an endpoint seen in one HAR capture
  from the web client. If DeepSeek changes that endpoint, the stop fails, the
  warning shows it, and the message is left `wip` again.
- What must be revisited if the stop fails in the field: capture a new HAR, and
  compare the request with `STOP_STREAM_PATH` in `crates/client/src/lib.rs`.

## Verification

- Client, `crates/client/src/completion.rs`, tests against a local HTTP server:
  `stop_names_the_message_the_stream_named`,
  `dropping_an_unfinished_completion_stops_it`,
  `a_finished_completion_is_not_stopped`.
- Proxy, `crates/proxy/src/sse.rs`: the closer-spam tests still pass. They use a
  base URL with nothing listening, so the stop fails and only logs a warning.
- Not verified here: a live stop against a real account. The manual check is
  `deepseek-rs check` followed by a reply that ends in a closer-spam run. The
  expected evidence is no `stop_stream failed` warning around the
  `upstream stream finished ... ended="closer spam"` line, and no `message still
  wip` on the next turn. A stop that fails logs `stop_stream failed`, and a stop
  that is dropped with the connection logs `stopped abandoned completion` at
  debug level.
