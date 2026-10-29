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
/// Debug capture of one upstream turn (`DEEPSEEK_SSE_DUMP=<dir>`).
///
/// `<id>.sse` is the raw bytes exactly as received; `<id>.routed` is what the
/// fragment router made of them, one line per piece, so a lost reply can be
/// found by comparing the two. Off unless the variable is set.
struct SseDump {
    raw: std::fs::File,
    routed: std::fs::File,
}

impl SseDump {
    fn open(id: &str) -> Option<Self> {
        let dir = std::path::PathBuf::from(std::env::var_os("DEEPSEEK_SSE_DUMP")?);
        std::fs::create_dir_all(&dir).ok()?;
        let open = |ext: &str| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join(format!("{id}.{ext}")))
                .ok()
        };
        Some(Self {
            raw: open("sse")?,
            routed: open("routed")?,
        })
    }

    fn raw(&mut self, bytes: &[u8]) {
        use std::io::Write;
        let _ = self.raw.write_all(bytes);
    }

    fn routed(&mut self, channel: &str, text: &str) {
        use std::io::Write;
        let _ = writeln!(self.routed, "{channel}\t{}", text.escape_debug());
    }
}

/// Decodes a byte stream whose network chunks may split a multi-byte character.
#[derive(Default)]
struct Utf8Joiner {
    pending: Vec<u8>,
}

impl Utf8Joiner {
    /// Decode `bytes` after anything held back from the previous chunk. A
    /// sequence cut off at the end of the chunk is kept for the next one; bytes
    /// that can never be valid UTF-8 become U+FFFD, as `from_utf8_lossy` does.
    fn push(&mut self, bytes: &[u8]) -> String {
        self.pending.extend_from_slice(bytes);
        let mut out = String::new();
        let mut consumed = 0;
        loop {
            match std::str::from_utf8(&self.pending[consumed..]) {
                Ok(valid) => {
                    out.push_str(valid);
                    consumed = self.pending.len();
                    break;
                }
                Err(e) => {
                    let end = consumed + e.valid_up_to();
                    out.push_str(&String::from_utf8_lossy(&self.pending[consumed..end]));
                    consumed = end;
                    match e.error_len() {
                        Some(bad) => {
                            out.push('\u{FFFD}');
                            consumed += bad;
                        }
                        // Incomplete character at the very end: wait for more.
                        None => break,
                    }
                }
            }
        }
        self.pending.drain(..consumed);
        out
    }
}

/// Append `text` to `head`, keeping at most `max` bytes without ever cutting a
/// character in half (`String::truncate` panics on a non-boundary, which killed
/// the worker whenever the first kilobyte of a stream straddled a multi-byte
/// character).
fn push_capped(head: &mut String, text: &str, max: usize) {
    head.push_str(text);
    if head.len() > max {
        let mut end = max;
        while !head.is_char_boundary(end) {
            end -= 1;
        }
        head.truncate(end);
    }
}

pub async fn pump(
    resp: reqwest::Response,
    tx: &ChunkSender,
    id: &str,
    model: &str,
    created: u64,
    idle: std::time::Duration,
    include_usage: bool,
    dialect: dialect::Dialect,
) -> Pumped {
    let mut router = FragmentRouter::new();
    let mut events = Vec::new();
    let mut buf = String::new();
    let mut current_event = String::new();
    let mut parent: Option<i64> = None;
    let mut filter = dialect::StreamToolFilter::new(dialect);
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
    let mut utf8 = Utf8Joiner::default();
    let mut dump = SseDump::open(id);
    // One debug summary per upstream stream, however it ends. Logging only:
    // `finish` captures nothing mutable, so control flow is unchanged.
    let started = std::time::Instant::now();
    let mut saw_done = false;
    // Assigned on every path that leaves the loop (a `break` or a `return`).
    let ended: &'static str;
    let finish = |ended: &'static str, bytes: usize, events: usize| {
        tracing::debug!(
            thread = %id,
            bytes,
            events,
            duration_ms = started.elapsed().as_millis() as u64,
            ended,
            "upstream stream finished"
        );
    };

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
                finish("client disconnected", raw_bytes, raw_events);
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
            Ok(None) => {
                ended = if saw_done { "[DONE]" } else { "upstream closed" };
                break;
            }
            Err(_) => {
                if tx
                    .send(Chunk::KeepAlive)
                    .await
                    .is_err()
                {
                    ended = "client disconnected";
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
                tracing::error!(
                    thread = %id,
                    cause = %crate::log::error_chain(&e),
                    "upstream stream read failed"
                );
                finish("read error", raw_bytes, raw_events);
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
        if let Some(d) = dump.as_mut() {
            d.raw(&bytes);
        }
        // Decode once, carrying an incomplete trailing character into the next
        // chunk: decoding each chunk alone turned a character split across two
        // chunks into two U+FFFD replacements, garbling text and tool calls.
        let text = utf8.push(&bytes);
        if head.len() < 1024 {
            push_capped(&mut head, &text, 1024);
        }
        buf.push_str(&text);
        while let Some(pos) = buf.find('\n') {
            let line = buf[..pos].to_string();
            buf.drain(..=pos);
            // classify_line drops `[DONE]` silently; note it for the summary.
            if line.trim().strip_prefix("data:").map(str::trim) == Some("[DONE]") {
                saw_done = true;
            }
            let value = match protocol::classify_line(&line, &mut current_event) {
                protocol::LineKind::Ignored => continue,
                protocol::LineKind::HintError { content } => {
                    finish("hint error", raw_bytes, raw_events);
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
            // First id wins. aiodeepseek's `stream_chat` freezes `message_id`
            // at the first id it extracts (`if message_id is None:`) and
            // `Conversation.ask_stream` parents the next turn to that value, so
            // a later id in the same stream must not replace it.
            if parent.is_none() {
                if let Some(mid) = value.get("response_message_id").and_then(|v| v.as_i64()) {
                    parent = Some(mid);
                }
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
                if let Some(d) = dump.as_mut() {
                    d.routed(
                        match ev.channel {
                            Channel::Text => "text",
                            Channel::Reasoning => "reasoning",
                        },
                        &ev.text,
                    );
                }
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
                    finish("client disconnected", raw_bytes, raw_events);
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

    finish(ended, raw_bytes, raw_events);

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

/// The backend's fatal `event: hint` error in a fully buffered reply, if it sent
/// one. The streaming pump stops at the first such hint; a buffered reply (a
/// correction turn) has to be checked after the fact. aiodeepseek's `ask`
/// raises on it before assigning the cursor, so the caller must treat the
/// reply as discarded: no cursor move, nothing accepted from it.
pub fn hint_in(raw: &str) -> Option<String> {
    let mut current_event = String::new();
    for line in raw.lines() {
        if let protocol::LineKind::HintError { content } =
            protocol::classify_line(line, &mut current_event)
        {
            return Some(content);
        }
    }
    None
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
        .next(); // first id wins, as in aiodeepseek's `stream_chat`
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The diagnostic head is capped in BYTES, but a stream is full of multi-byte
    /// characters (CJK text, emoji, the fullwidth bar of the tool markers).
    /// Cutting inside one panicked the worker and broke the stream.
    #[test]
    fn push_capped_never_splits_a_character() {
        // 1023 bytes, then a 3-byte character that straddles byte 1024.
        let mut head = "a".repeat(1023);
        push_capped(&mut head, "\u{FF5C}\u{FF5C}", 1024);
        assert!(head.len() <= 1024);
        assert!(head.is_char_boundary(head.len()));
        assert_eq!(head, "a".repeat(1023));
    }

    #[test]
    fn push_capped_keeps_whole_characters_up_to_the_cap() {
        let mut head = String::new();
        push_capped(&mut head, &"\u{FF5C}".repeat(400), 1024);
        // 341 whole 3-byte characters = 1023 bytes.
        assert_eq!(head.len(), 1023);
        let mut short = String::from("ab");
        push_capped(&mut short, "cd", 1024);
        assert_eq!(short, "abcd");
    }

    #[test]
    fn a_character_split_across_chunks_is_decoded_whole() {
        let bytes = "a\u{FF5C}b".as_bytes(); // 'a', EF BD 9C, 'b'
        for split in 1..bytes.len() {
            let mut j = Utf8Joiner::default();
            let mut out = j.push(&bytes[..split]);
            out.push_str(&j.push(&bytes[split..]));
            assert_eq!(out, "a\u{FF5C}b", "split at {split}");
        }
    }

    #[test]
    fn one_byte_at_a_time_still_decodes_cjk_and_emoji() {
        let text = "ok \u{4F60}\u{597D} \u{1F600} done";
        let mut j = Utf8Joiner::default();
        let mut out = String::new();
        for b in text.as_bytes() {
            out.push_str(&j.push(std::slice::from_ref(b)));
        }
        assert_eq!(out, text);
    }

    #[test]
    fn genuinely_invalid_bytes_are_still_replaced() {
        let mut j = Utf8Joiner::default();
        let out = j.push(b"a\xFFb");
        assert_eq!(out, "a\u{FFFD}b");
    }
}
