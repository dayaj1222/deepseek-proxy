//! Upstream SSE pump: read one completion stream and re-emit OpenAI chunks.
//!
//! Moved out of `main.rs` unchanged. It is the one place that knows how a
//! DeepSeek event becomes `chat.completion.chunk` deltas, so the streaming
//! half of a turn has a module of its own instead of living inside a handler.

use axum::response::sse::Event;
use futures_util::StreamExt;
use protocol::{route_event, Channel, FragmentRouter};
use serde_json::{json, Value};
use tokio_stream::wrappers::ReceiverStream;

use crate::dialect;

/// One thing a turn produces before it is turned into a transport.
///
/// The turn is the same whether the client asked for SSE or one JSON body, so
/// it emits these and the caller decides: the streaming path wraps each one in
/// an [`Event`], the non-streaming path concatenates them into a
/// `chat.completion`. Errors travel in-band because a turn can fail after the
/// first byte, and only the caller knows whether a status code is still
/// available to change.
pub enum Chunk {
    /// An OpenAI-shaped `chat.completion.chunk` payload.
    Data(String),
    /// SSE keep-alive comment; ignored by the non-streaming collector.
    KeepAlive,
    /// Terminal failure: HTTP status plus the OpenAI error envelope.
    Error(axum::http::StatusCode, Value),
}

/// Channel a turn writes to; the caller owns the receiving half.
pub type ChunkSender = tokio::sync::mpsc::Sender<Chunk>;
pub type ChunkReceiver = tokio::sync::mpsc::Receiver<Chunk>;

/// Wrap a chunk receiver as an SSE response body.
pub fn sse_stream(rx: ChunkReceiver) -> impl futures_util::Stream<Item = Result<Event, std::convert::Infallible>> {
    ReceiverStream::new(rx).map(|chunk| {
        Ok(match chunk {
            Chunk::Data(data) => Event::default().data(data),
            Chunk::KeepAlive => Event::default().comment("keep-alive"),
            // The status cannot be sent at this point; the envelope still can,
            // which is what the client parses.
            Chunk::Error(_, payload) => Event::default().data(payload.to_string()),
        })
    })
}

/// Result of draining one upstream SSE response into OpenAI chunks.
pub struct Pumped {
    pub parent: Option<i64>,
    pub filter: dialect::StreamToolFilter,
    /// True once any chunk reached the client; a hint after that is fatal.
    pub emitted: bool,
    /// Fatal `event: hint` payload, if the upstream aborted the stream.
    pub hint: Option<String>,
    /// Every text fragment seen, for the malformed-call detector. The filter
    /// keeps only parseable calls, so the raw text must be kept separately.
    pub text: String,
    /// Prose actually forwarded to the client (tool blocks excluded). Python's
    /// `Completion.text`; used for the `usage` completion-token count.
    pub prose: String,
    /// Upstream `accumulated_token_usage` (prompt side), when the stream
    /// reported one. Python reads the same field off its `done` event.
    pub prompt_tokens: u64,
}

/// Latest `accumulated_token_usage` anywhere in an event, if present.
///
/// The backend sends it twice: on the initial `response` object and as a
/// `BATCH` child patch (`{"p":"accumulated_token_usage","v":51}`).
pub fn find_token_usage(value: &Value) -> Option<u64> {
    if let Some(n) = value
        .get("accumulated_token_usage")
        .and_then(|v| v.as_u64())
    {
        return Some(n);
    }
    if value.get("p").and_then(|p| p.as_str()) == Some("accumulated_token_usage") {
        return value.get("v").and_then(|v| v.as_u64());
    }
    match value {
        Value::Object(map) => map.values().find_map(find_token_usage),
        Value::Array(items) => items.iter().find_map(find_token_usage),
        _ => None,
    }
}

/// Token estimate for a piece of text, matching Python's `estimate_tokens`
/// (tiktoken `cl100k_base`). Falls back to a 4-chars-per-token heuristic when
/// the encoding cannot be loaded.
///
/// The BPE table is loaded once and reused. `tiktoken_rs::cl100k_base()`
/// rebuilds it on every call (tens of milliseconds); this runs once per turn
/// on every completion, so the rebuild was pure waste.
pub fn estimate_tokens(text: &str) -> u64 {
    if text.is_empty() {
        return 0;
    }
    static BPE: std::sync::OnceLock<Option<tiktoken_rs::CoreBPE>> = std::sync::OnceLock::new();
    match BPE.get_or_init(|| tiktoken_rs::cl100k_base().ok()) {
        Some(bpe) => bpe.encode_with_special_tokens(text).len() as u64,
        None => (text.chars().count() as u64).div_ceil(4),
    }
}

/// OpenAI `usage` block.
///
/// Field provenance, verified against a live two-turn probe on one thread:
///
/// - `prompt_tokens` is forwarded verbatim from the backend's
///   `accumulated_token_usage`, the only token number DeepSeek sends. Measured
///   on the wire it is **prompt/input-side, not total**: a turn whose answer was
///   273 tokens was followed by a prompt that grew by only 26. So it maps to
///   `prompt_tokens` and must NOT be treated as a total (subtracting the
///   completion from it would under-count by a whole turn).
/// - `completion_tokens` has no upstream source at all — DeepSeek never reports
///   output tokens — so it is estimated locally with `cl100k_base`. This is the
///   one number that is self-counted, and it is approximate.
/// - `total_tokens` is derived from the two.
///
/// Note this deliberately counts *prose only*: reasoning is excluded, matching
/// Python's `Completion.usage()`. OpenAI would include it in `completion_tokens`.
pub fn usage_block(prompt_tokens: u64, text: &str, calls: &[dialect::ToolCall]) -> Value {
    let mut completion = estimate_tokens(text);
    for call in calls {
        completion += estimate_tokens(&format!("{}{}", call.name, call.arguments));
    }
    json!({
        "prompt_tokens": prompt_tokens,
        "completion_tokens": completion,
        "total_tokens": prompt_tokens + completion,
    })
}

/// Read one upstream stream to completion, forwarding chunks to `tx`.
///
/// Two deadlines guard this, both from the Python proxy:
///
/// - `idle` (the heartbeat interval) bounds how long we wait for the *next*
///   byte, and emits `: keep-alive` while we wait. Python does this with
///   `asyncio.wait_for(queue.get(), heartbeat_interval)`.
/// - The caller bounds the whole turn with `request_timeout`.
pub async fn pump(
    resp: reqwest::Response,
    tx: &ChunkSender,
    id: &str,
    model: &str,
    created: u64,
    idle: std::time::Duration,
    include_usage: bool,
) -> Pumped {
    let mut router = FragmentRouter::new();
    let mut events = Vec::new();
    let mut buf = String::new();
    let mut current_event = String::new();
    let mut parent: Option<i64> = None;
    let mut filter = dialect::StreamToolFilter::new();
    let mut emitted = false;
    let mut text_all = String::new();
    let mut prose = String::new();
    let mut prompt_tokens: u64 = 0;
    // Diagnostics for the "clean stream, no text" failure: the upstream can
    // answer with something we cannot route (an HTML/JSON error page on a 200,
    // or fragments in an unexpected shape). Without the raw bytes the only
    // visible symptom is "closed the stream without content", which says
    // nothing about which of those it was.
    let mut raw_bytes: usize = 0;
    let mut raw_events: usize = 0;
    let mut head = String::new();

    let mut byte_stream = resp.bytes_stream();
    loop {
        // Python's `stream()` waits `heartbeat_interval` for the next event and
        // emits `: keep-alive` on timeout — it does NOT kill the stream. The
        // turn-wide `request_timeout` is what bounds a real hang.
        let next = tokio::select! {
            biased;
            // The client hung up. Stop reading immediately: otherwise this task
            // stays parked on the socket forever, never releases the account's
            // rate slot, and every later request on this account blocks in
            // `acquire()` — silently, with no log line. Python gets the same
            // effect from `producer.cancel()` in the stream's `finally`.
            _ = tx.closed() => {
                tracing::info!(thread = %id, "client disconnected, stopping upstream read");
                return Pumped {
                    parent,
                    filter,
                    emitted,
                    hint: None,
                    text: text_all,
                    prose,
                    prompt_tokens,
                };
            }
            r = tokio::time::timeout(idle, byte_stream.next()) => r,
        };
        let chunk = match next {
            Ok(Some(chunk)) => chunk,
            Ok(None) => break,
            Err(_) => {
                if tx
                    .send(Chunk::KeepAlive)
                    .await
                    .is_err()
                {
                    break;
                }
                continue;
            }
        };
        // A read error is a real failure. The old `let Ok(..) else { break }`
        // turned a dead socket into a clean, empty, *successful* stream — which
        // is why stalled turns produced no log line and no error at all.
        let bytes = match chunk {
            Ok(b) => b,
            Err(e) => {
                tracing::error!(thread = %id, error = %e, "upstream stream read failed");
                return Pumped {
                    parent,
                    filter,
                    emitted,
                    hint: Some(format!("stream read failed: {e}")),
                    text: text_all,
                    prose,
                    prompt_tokens,
                };
            }
        };
        raw_bytes += bytes.len();
        if head.len() < 1024 {
            head.push_str(&String::from_utf8_lossy(&bytes));
            if head.len() > 1024 {
                head.truncate(1024);
            }
        }
        buf.push_str(&String::from_utf8_lossy(&bytes));
        while let Some(pos) = buf.find('\n') {
            let line = buf[..pos].to_string();
            buf.drain(..=pos);
            let value = match protocol::classify_line(&line, &mut current_event) {
                protocol::LineKind::Ignored => continue,
                protocol::LineKind::HintError { content } => {
                    return Pumped {
                        parent,
                        filter,
                        emitted,
                        hint: Some(content),
                        text: text_all,
                        prose,
                        prompt_tokens,
                    };
                }
                protocol::LineKind::Event(value) => value,
            };
            // Last id wins. aiodeepseek's `ask_stream` keeps the final
            // non-None `message_id` it sees and parents the next turn to that,
            // so an early id must not win here either.
            if let Some(mid) = value.get("response_message_id").and_then(|v| v.as_i64()) {
                parent = Some(mid);
            }
            // `accumulated_token_usage` is patched mid-stream, once as a plain
            // field and again as a BATCH child. Keep the latest value; Python
            // reads it off the same field when building its `done` event.
            if let Some(n) = find_token_usage(&value) {
                prompt_tokens = n;
            }
            raw_events += 1;
            events.clear();
            route_event(&value, &mut router, &mut events);
            for ev in &events {
                let delta = match ev.channel {
                    Channel::Text => {
                        text_all.push_str(&ev.text);
                        // Tool blocks are held back and surfaced as tool_calls.
                        let safe = filter.feed(&ev.text);
                        if safe.is_empty() {
                            continue;
                        }
                        prose.push_str(&safe);
                        json!({ "content": safe })
                    }
                    Channel::Reasoning => json!({ "reasoning_content": ev.text }),
                };
                let mut chunk = json!({
                    "id": id,
                    "object": "chat.completion.chunk",
                    "created": created,
                    "model": model,
                    "choices": [{"index": 0, "delta": delta, "finish_reason": null}]
                });
                // OpenAI: with `include_usage`, every content chunk carries
                // `usage: null`; the real numbers arrive in a final chunk.
                if include_usage {
                    chunk["usage"] = Value::Null;
                }
                emitted = true;
                if tx
                    .send(Chunk::Data(chunk.to_string()))
                    .await
                    .is_err()
                {
                    return Pumped {
                        parent,
                        filter,
                        emitted,
                        hint: None,
                        text: text_all,
                        prose,
                        prompt_tokens,
                    };
                }
            }
        }
    }

    if text_all.is_empty() {
        tracing::error!(
            thread = %id,
            raw_bytes,
            raw_events,
            head = %head.replace('\n', "\\n"),
            "upstream stream ended with no routable text; raw body above"
        );
    }

    // A 200 response that is not an SSE stream at all (a JSON `{code,biz_msg}`
    // error body) used to fall through as "empty response", hiding the reason.
    // Surface the body as a hint so the caller can classify it: `code:26`
    // means the parent id is stale and the turn is retryable.
    let hint = if raw_events == 0 && !head.trim().is_empty() {
        Some(format!("non-SSE upstream body: {}", head.trim()))
    } else {
        None
    };

    Pumped {
        parent,
        filter,
        emitted,
        hint,
        text: text_all,
        prose,
        prompt_tokens,
    }
}

pub fn route_all(raw: &str) -> (String, String, Option<i64>) {
    let events = protocol::route_stream(raw);
    let mut reasoning = String::new();
    let mut answer = String::new();
    for ev in events {
        match ev.channel {
            Channel::Text => answer.push_str(&ev.text),
            Channel::Reasoning => reasoning.push_str(&ev.text),
        }
    }
    let mid = raw
        .lines()
        .filter_map(|l| l.trim().strip_prefix("data:"))
        .filter_map(|p| serde_json::from_str::<Value>(p.trim()).ok())
        .filter_map(|v| v.get("response_message_id").and_then(|x| x.as_i64()))
        .last();
    (reasoning, answer, mid)
}

pub fn uuid_like() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{n:x}")
}

pub fn unix_secs() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
