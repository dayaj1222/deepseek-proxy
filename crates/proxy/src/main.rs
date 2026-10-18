//! OpenAI-compatible proxy over the DeepSeek private API.
//!
//! Rust port of `deepseek_proxy`. Serves:
//!
//! - `GET  /healthz`
//! - `GET  /v1/models`
//! - `POST /v1/chat/completions` (stream + non-stream)
//!
//! ## Session model
//!
//! DeepSeek keeps conversation state server-side, so a thread is just
//! `(chat_session_id, parent_message_id)`. Threads are keyed by an explicit
//! `thread_id`, then the `x-thread-id` header, then the client session id
//! (`x-session-affinity` / `x-session-id`), and only then by a message hash —
//! the same precedence as the Python proxy. Continuation turns send only the
//! newest user message, matching the Python proxy's delta behaviour; a fresh
//! thread sends the flattened transcript.
//!
//! ## Not yet ported
//!
//! - SQLite persistence of threads (in-memory only; lost on restart)
//! - `tools/format.py` prompt + tool-call dialect injection
//! - multi-account failover

mod cli;
mod config;
mod dialect;
mod log;
mod pool;
mod store;

use axum::{
    extract::State,
    http::HeaderMap,
    response::{
        sse::{Event, Sse},
        IntoResponse, Response,
    },
    routing::{get, post},
    Json, Router,
};
use client::{CompletionBody, COMPLETION_PATH};
use futures_util::StreamExt;
use protocol::{route_event, Channel, FragmentRouter};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::convert::Infallible;
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio_stream::wrappers::ReceiverStream;

#[derive(Clone)]
struct Settings {
    thinking_default: bool,
    search_enabled: bool,
    model_name: String,
    tool_reminder_interval: usize,
}

#[derive(Debug, Clone)]
struct ThreadState {
    session_id: String,
    parent_message_id: Option<i64>,
    /// User/tool messages exchanged so far on this thread. Drives the periodic
    /// tool-format reminder; mirrors Python's `thread_exchanges` counter.
    exchanges: usize,
}

#[derive(Clone)]
struct AppState {
    pool: Arc<pool::Pool>,
    settings: Settings,
    threads: Arc<Mutex<HashMap<String, ThreadState>>>,
    store: store::Store,
    stats: Arc<Stats>,
}

/// Process counters reported by `GET /stats`. Cheap atomics only; anything
/// expensive (account gates, db counts) is read live by the handler.
#[derive(Default)]
struct Stats {
    started: Option<Instant>,
    total: AtomicU64,
    ok: AtomicU64,
    errors: AtomicU64,
    auth_failed: AtomicU64,
    retries: AtomicU64,
    tool_calls: AtomicU64,
}

impl Stats {
    fn new() -> Self {
        Self {
            started: Some(Instant::now()),
            ..Default::default()
        }
    }

    fn uptime_secs(&self) -> u64 {
        self.started.map(|t| t.elapsed().as_secs()).unwrap_or(0)
    }
}

#[derive(Debug, Deserialize)]
struct ChatRequest {
    #[serde(default)]
    model: Option<String>,
    /// Explicit conversation key, mirroring the Python proxy's `thread_id`.
    #[serde(default)]
    thread_id: Option<String>,
    #[serde(default)]
    messages: Vec<Msg>,
    #[serde(default)]
    stream: bool,
    #[serde(default)]
    reasoning_effort: Option<String>,
    #[serde(default)]
    thinking: Option<Value>,
    #[serde(default)]
    reasoning: Option<Value>,
    #[serde(default)]
    tools: Vec<Value>,
}

#[derive(Debug, Deserialize)]
struct Msg {
    role: String,
    #[serde(default)]
    content: Option<Value>,
    // Deserialized for completeness; the prompt builders only need role/content.
    #[serde(default)]
    #[allow(dead_code)]
    tool_calls: Vec<Value>,
    #[serde(default)]
    #[allow(dead_code)]
    tool_call_id: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

/// Prompts for one turn: the delta the session needs now, and the full
/// flattened transcript used to rebuild the session when it overflows.
///
/// Mirrors the Python proxy's `prompt` / `replay_prompt` pair.
fn build_prompts(
    req: &ChatRequest,
    existing: Option<&ThreadState>,
    system_extra: &str,
    reminder_interval: usize,
) -> (String, String) {
    let transcript = flatten_transcript(&req.messages);
    let replay = if system_extra.is_empty() {
        transcript
    } else {
        format!("{transcript}\n\n{system_extra}")
    };
    let mut delta = match existing {
        Some(_) => continuation_prompt(&req.messages).unwrap_or_default(),
        None => replay.clone(),
    };
    // Re-anchor the wire format every N user/tool messages. The system block
    // is only sent on the first turn, so without this a long thread drifts off
    // the format and starts emitting malformed blocks.
    let reminder_needed = if reminder_interval == 1 {
        // Per-turn: every continuation turn carries tools re-anchors the format.
        existing.is_some()
    } else {
        reminder_due(existing, &req.messages, reminder_interval)
    };
    if !req.tools.is_empty() && reminder_needed {
        delta.push_str("\n\n");
        delta.push_str(&dialect::periodic_reminder());
    }
    (delta, replay)
}

/// Count user and tool messages, matching Python's `sum(role in (user, tool))`.
fn counted_messages(messages: &[Msg]) -> usize {
    // pi resends the whole transcript every turn, so counting every message
    // would grow the counter quadratically. Count only this turn's delta, the
    // same slice `continuation_prompt` forwards and Python counts.
    let last_assistant = messages.iter().rposition(|m| m.role == "assistant");
    let delta = match last_assistant {
        Some(i) => &messages[i + 1..],
        None => messages,
    };
    delta
        .iter()
        .filter(|m| m.role == "user" || m.role == "tool")
        .count()
}

/// True when this turn crosses a reminder interval boundary.
///
/// Mirrors Python: `(prev + count) / interval > prev / interval`. A fresh
/// thread starts at zero, and `interval == 0` disables the reminder.
fn reminder_due(existing: Option<&ThreadState>, messages: &[Msg], interval: usize) -> bool {
    if interval == 0 {
        return false;
    }
    let prev = existing.map(|t| t.exchanges).unwrap_or(0);
    (prev + counted_messages(messages)) / interval > prev / interval
}

fn completion_body(
    thread: &ThreadState,
    prompt: &str,
    thinking: bool,
    search: bool,
) -> CompletionBody {
    let mut body = CompletionBody::new(&thread.session_id, prompt);
    body.thinking_enabled = thinking;
    body.search_enabled = search;
    body.parent_message_id = thread.parent_message_id;
    body
}

/// First fatal `hint` error in a fully-buffered SSE body, if any.
fn first_hint(raw: &str) -> Option<String> {
    let mut current = String::new();
    for line in raw.lines() {
        if let protocol::LineKind::HintError { content } = protocol::classify_line(line, &mut current) {
            return Some(content);
        }
    }
    None
}

/// Replace the remote session after a context-limit rejection.
///
/// DeepSeek keeps the conversation server-side, so an overflowing session
/// cannot be trimmed from here: create a fresh one and let the caller replay
/// the whole transcript into it. Persisted resume state is reset to match.
async fn rebuild_session(
    state: &AppState,
    key: &str,
    reason: &str,
) -> Result<ThreadState, Response> {
    let conn = state.pool.route(key).await;
    let session_id = {
        let _slot = conn
            .acquire()
            .await
            .map_err(|e| backend_error(&format!("rate slot: {e}")))?;
        conn.client()
            .await
            .create_chat_session()
            .await
            .map_err(|e| backend_error(&format!("create_session: {e}")))?
    };
    tracing::warn!("thread {key}: {reason}; rebuilding in a new remote session");

    let fresh = ThreadState {
        session_id,
        parent_message_id: None,
        // A rebuilt session replays the whole transcript, so the counters
        // restart with it.
        exchanges: 0,
    };
    state
        .threads
        .lock()
        .unwrap()
        .insert(key.to_string(), fresh.clone());
    let _ = state.store.save_thread(
        key,
        &store::ThreadResume {
            session_id: fresh.session_id.clone(),
            parent_message_id: None,
        },
    );
    Ok(fresh)
}

fn content_text(content: &Option<Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Resolve the per-request thinking intent. Precedence mirrors the Python
/// proxy: `reasoning_effort` > `reasoning` object > `thinking` object > config.
fn want_thinking(req: &ChatRequest, default: bool) -> bool {
    const OFF: [&str; 4] = ["none", "off", "false", "disabled"];
    const ON: [&str; 6] = ["low", "medium", "high", "on", "true", "enabled"];

    if let Some(effort) = &req.reasoning_effort {
        let e = effort.trim().to_ascii_lowercase();
        if OFF.contains(&e.as_str()) {
            return false;
        }
        if ON.contains(&e.as_str()) {
            return true;
        }
    }
    for obj in [&req.reasoning, &req.thinking].into_iter().flatten() {
        if let Some(inner) = obj
            .get("effort")
            .or_else(|| obj.get("enabled"))
            .or_else(|| obj.get("type"))
        {
            if let Some(s) = inner.as_str() {
                let s = s.trim().to_ascii_lowercase();
                if OFF.contains(&s.as_str()) {
                    return false;
                }
                if ON.contains(&s.as_str()) || s.starts_with("enabled") {
                    return true;
                }
            } else if let Some(b) = inner.as_bool() {
                return b;
            }
        }
    }
    default
}

/// Client-advertised session identity headers, mirroring the Python proxy's
/// `_SESSION_HEADERS`. pi sends `X-Session-Id`/`session_id`, opencode sends
/// `x-session-affinity`. Their session UUID survives compaction, which the
/// first-message hash does not.
const SESSION_HEADERS: [&str; 4] = [
    "x-session-affinity",
    "x-opencode-session",
    "x-session-id",
    "session_id",
];

/// Same sentinel the Python proxy uses to spot Hermes' session-title calls.
const TITLE_PROMPT_MARKER: &str = "You name chat sessions";

fn sha256_hex(prefix: &str, seed: &str, user: Option<&str>) -> String {
    let mut payload = String::new();
    if let Some(user) = user.filter(|u| !u.is_empty()) {
        payload.push_str(user);
        payload.push('\0');
    }
    payload.push_str(seed);
    let digest = Sha256::digest(payload.as_bytes());
    let hexed = hex::encode(digest);
    format!("{prefix}{}", &hexed[..24])
}

/// Title-generation calls inherit the parent session's headers, so they must
/// get their own namespace or their responses cross with the real chat.
fn is_title_request(req: &ChatRequest) -> bool {
    req.messages
        .iter()
        .filter(|m| m.role == "system" || m.role == "developer")
        .any(|m| content_text(&m.content).starts_with(TITLE_PROMPT_MARKER))
}

fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Resolve the conversation key for a request.
///
/// Precedence mirrors the Python proxy: explicit `thread_id` body field, then
/// the `x-thread-id` header, then the client session id (hashed, so a stable
/// UUID keeps one DeepSeek conversation across compaction), and only then a
/// hash of the messages. The old message-only hash stranded a conversation on
/// a fresh remote session every time the first message changed.
fn thread_key(headers: &HeaderMap, req: &ChatRequest) -> String {
    if let Some(v) = req.thread_id.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
        return v.to_string();
    }
    if let Some(v) = header_value(headers, "x-thread-id") {
        return v;
    }
    // Title calls must be detected before the session id is consulted.
    let prefix = if is_title_request(req) { "title_" } else { "thread_" };
    if let Some(session) = SESSION_HEADERS
        .iter()
        .find_map(|name| header_value(headers, name))
    {
        return sha256_hex(prefix, &session, None);
    }
    if let Some(first) = req
        .messages
        .iter()
        .find(|m| m.role == "user" && !content_text(&m.content).is_empty())
    {
        return sha256_hex(prefix, &content_text(&first.content), None);
    }
    let all: String = req
        .messages
        .iter()
        .map(|m| content_text(&m.content))
        .collect();
    sha256_hex(prefix, &all, None)
}

/// Fresh thread: flatten the whole conversation.
fn flatten_transcript(messages: &[Msg]) -> String {
    let mut out = String::new();
    for m in messages {
        let text = content_text(&m.content);
        if text.is_empty() {
            continue;
        }
        out.push_str(&format!("[{}] {}\n\n", m.role, text));
    }
    out.trim_end().to_string()
}

/// Existing thread: send only the newest user message.
fn newest_user_message(messages: &[Msg]) -> Option<String> {
    messages
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .map(|m| content_text(&m.content))
        .filter(|s| !s.is_empty())
}

/// Build the continuation prompt for a thread that already exists.
///
/// Handles the tool round trip: after the model emits a tool call, the client
/// sends back `role: "tool"` messages with the results. Those must be
/// forwarded, otherwise the model never sees what its call returned.
fn continuation_prompt(messages: &[Msg]) -> Option<String> {
    // Tool results are the most recent thing to forward when present.
    let tool_results: Vec<String> = messages
        .iter()
        .rev()
        .take_while(|m| m.role == "tool" || m.role == "assistant")
        .filter(|m| m.role == "tool")
        .map(|m| {
            let body = content_text(&m.content);
            let label = m.name.as_deref().unwrap_or("tool");
            format!("Tool result ({label}):\n{body}")
        })
        .collect();

    if !tool_results.is_empty() {
        let mut joined = tool_results;
        joined.reverse();
        return Some(joined.join("\n\n"));
    }

    newest_user_message(messages)
}

async fn healthz() -> &'static str {
    "ok"
}

/// `GET /stats` — everything `deepseek-rs stats` and `status` need.
async fn stats(State(state): State<AppState>) -> Json<Value> {
    let s = &state.stats;
    let counts = state.store.counts().unwrap_or_default();
    let threads_in_memory = state.threads.lock().unwrap().len();

    let accounts: Vec<Value> = state
        .pool
        .snapshot()
        .await
        .into_iter()
        .map(|a| {
            json!({
                "email": a.email,
                "healthy": a.healthy,
                "bound_threads": a.bound_threads,
                "waiting": a.waiting,
                "cooldown_ms": a.cooldown_ms,
            })
        })
        .collect();

    Json(json!({
        "pid": std::process::id(),
        "uptime_secs": s.uptime_secs(),
        "model": state.settings.model_name,
        "thinking": state.settings.thinking_default,
        "search": state.settings.search_enabled,
        "requests": {
            "total": s.total.load(Ordering::Relaxed),
            "ok": s.ok.load(Ordering::Relaxed),
            "errors": s.errors.load(Ordering::Relaxed),
            "auth_failed": s.auth_failed.load(Ordering::Relaxed),
            "retries": s.retries.load(Ordering::Relaxed),
            "tool_calls": s.tool_calls.load(Ordering::Relaxed),
        },
        "threads": { "in_memory": threads_in_memory },
        "db": {
            "threads": counts.threads,
            "bindings": counts.bindings,
            "api_keys": counts.api_keys,
            "accounts": counts.accounts,
            "cached_tokens": counts.cached_tokens,
        },
        "accounts": accounts,
    }))
}

async fn models(State(state): State<AppState>) -> Json<Value> {
    let m = &state.settings.model_name;
    Json(json!({
        "object": "list",
        "data": [
            {"id": m, "object": "model", "owned_by": "deepseek"},
            {"id": "DEFAULT", "object": "model", "owned_by": "deepseek"},
            {"id": "EXPERT", "object": "model", "owned_by": "deepseek"},
        ]
    }))
}

/// Reject the request unless the bearer key is known (or none are registered).
fn authorize(state: &AppState, headers: &HeaderMap) -> Result<(), Response> {
    let presented = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim_start_matches("Bearer ").trim().to_string())
        .unwrap_or_default();

    match state.store.verify_api_key(&presented) {
        Ok(true) => Ok(()),
        Ok(false) => Err((
            axum::http::StatusCode::UNAUTHORIZED,
            Json(json!({"error": {"message": "invalid api key", "type": "auth_error"}})),
        )
            .into_response()),
        Err(e) => Err(backend_error(&format!("store: {e}"))),
    }
}

/// Send the completion, retrying transient rate limits before the first byte.
///
/// DeepSeek returns `rate_limit_reached` when thinking is used too often. Only
/// pre-first-byte failures are retried, which is safe for both stream and
/// non-stream callers; a mid-stream failure is surfaced as-is.
async fn completion_with_retry(
    state: &AppState,
    thread_id: &str,
    body: &CompletionBody,
) -> Result<reqwest::Response, String> {
    const ATTEMPTS: u32 = 3;
    let mut last = String::new();
    let mut conn = state.pool.route(thread_id).await;

    for attempt in 0..ATTEMPTS {
        // Hold this account's rate slot for the whole request; dropping it
        // records completion time so the next request waits the full delay.
        let slot = conn.acquire().await?;
        let client = conn.client().await;

        match client.stream_completion(body).await {
            Ok(resp) => return Ok(resp),
            Err(e) => {
                last = e.to_string();

                if pool::is_login_dead(&last) {
                    tracing::warn!("account {} login-dead: {last}", conn.email);
                    drop(slot);
                    match state.pool.failover(thread_id).await {
                        Some(next) => {
                            tracing::warn!("thread failed over to {}", next.email);
                            conn = next;
                            continue;
                        }
                        None => break,
                    }
                }

                if pool::is_invalid_token(&last) && state.pool.recover(&conn).await {
                    tracing::info!("recovered account {}", conn.email);
                    drop(slot);
                    continue;
                }

                let retryable = last.contains("rate_limit")
                    || last.contains("too frequent")
                    || last.contains("generation_err")
                    || last.contains("temporarily unavailable");
                drop(slot);
                if !retryable || attempt + 1 == ATTEMPTS {
                    break;
                }
                let wait_ms = 1000u64 * (1 << attempt);
                state.stats.retries.fetch_add(1, Ordering::Relaxed);
                tracing::warn!("completion failed ({last}); retrying in {wait_ms}ms");
                tokio::time::sleep(std::time::Duration::from_millis(wait_ms)).await;
            }
        }
    }
    Err(last)
}

async fn chat_completions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ChatRequest>,
) -> Response {
    if let Err(resp) = authorize(&state, &headers) {
        state.stats.auth_failed.fetch_add(1, Ordering::Relaxed);
        tracing::warn!("rejected request: invalid api key");
        return resp;
    }

    let started = std::time::Instant::now();
    let key = thread_key(&headers, &req);
    let model = req
        .model
        .clone()
        .unwrap_or_else(|| state.settings.model_name.clone());
    tracing::info!(
        thread = %key,
        model = %model,
        stream = req.stream,
        messages = req.messages.len(),
        tools = req.tools.len(),
        "request in"
    );
    state.stats.total.fetch_add(1, Ordering::Relaxed);

    // In-memory cache first, then SQLite (survives restarts).
    let existing = state
        .threads
        .lock()
        .unwrap()
        .get(&key)
        .cloned()
        .or_else(|| {
            state.store.get_thread(&key).ok().flatten().map(|r| ThreadState {
                session_id: r.session_id,
                parent_message_id: r.parent_message_id.and_then(|p| p.parse().ok()),
                exchanges: 0,
            })
        });

    // Tool definitions are taught once, on the first turn of a thread. Later
    // turns ride on the server-side conversation, like the Python proxy.
    let system_extra = dialect::inject_tool_descriptions("", &req.tools);

    let (prompt, replay) = build_prompts(
        &req,
        existing.as_ref(),
        &system_extra,
        state.settings.tool_reminder_interval,
    );
    if prompt.is_empty() {
        tracing::warn!(thread = %key, "rejecting turn: no user message or tool result");
        return bad_request("no user message or tool result in continuation turn");
    }
    tracing::debug!(
        thread = %key,
        fresh = existing.is_none(),
        prompt_chars = prompt.len(),
        replay_chars = replay.len(),
        "turn built"
    );
    // A brand-new thread has nothing to replay into, so a limit hit is final.
    let can_recover = existing.is_some() && !replay.is_empty();

    // Create the remote session lazily, on the first turn for this thread.
    let mut thread = match existing {
        Some(t) => t,
        None => {
            let conn = state.pool.route(&key).await;
            let session_id = {
                let _slot = match conn.acquire().await {
                    Ok(s) => s,
                    Err(e) => return backend_error(&format!("rate slot: {e}")),
                };
                let client = conn.client().await;
                match client.create_chat_session().await {
                    Ok(s) => s,
                    Err(e) => return backend_error(&format!("create_session: {e}")),
                }
            };
            let t = ThreadState {
                session_id,
                parent_message_id: None,
                exchanges: 0,
            };
            tracing::info!(thread = %key, session = %t.session_id, "new remote session");
            state
                .threads
                .lock()
                .unwrap()
                .insert(key.clone(), t.clone());
            t
        }
    };

    let thinking = want_thinking(&req, state.settings.thinking_default);
    let mut body = completion_body(&thread, &prompt, thinking, state.settings.search_enabled);

    let id = format!("chatcmpl-{}", uuid_like());
    let created = unix_secs();

    let resp = match completion_with_retry(&state, &key, &body).await {
        Ok(r) => r,
        Err(e) => return backend_error(&format!("completion: {e}")),
    };

    // Count this turn's user/tool messages so the next prompt can decide
    // whether a format reminder is due.
    if let Some(t) = state.threads.lock().unwrap().get_mut(&key) {
        t.exchanges = t.exchanges.saturating_add(counted_messages(&req.messages));
    }

    tracing::debug!(thread = %key, thinking, parent = ?thread.parent_message_id, "upstream completion accepted");

    if req.stream {
        return stream_response(
            state,
            key,
            resp,
            body,
            id,
            model,
            created,
            replay,
            can_recover,
            started,
        )
        .await;
    }

    // Non-stream: buffer everything, then answer in one shot.
    let raw = match resp.text().await {
        Ok(t) => t,
        Err(e) => return backend_error(&format!("read body: {e}")),
    };
    let raw = match first_hint(&raw) {
        Some(hint) if can_recover && protocol::is_context_limit(&hint) => {
            thread = match rebuild_session(&state, &key, &hint).await {
                Ok(t) => t,
                Err(resp) => return resp,
            };
            body = completion_body(&thread, &replay, thinking, state.settings.search_enabled);
            let resp = match completion_with_retry(&state, &key, &body).await {
                Ok(r) => r,
                Err(e) => return backend_error(&format!("completion retry: {e}")),
            };
            match resp.text().await {
                Ok(t) => t,
                Err(e) => return backend_error(&format!("read body: {e}")),
            }
        }
        _ => raw,
    };
    let (reasoning, answer, message_id) = route_all(&raw);
    let tool_calls = dialect::extract_tool_calls(&answer);
    state
        .stats
        .tool_calls
        .fetch_add(tool_calls.len() as u64, Ordering::Relaxed);
    state.stats.ok.fetch_add(1, Ordering::Relaxed);
    tracing::info!(
        thread = %key,
        reasoning_chars = reasoning.len(),
        answer_chars = answer.len(),
        tool_calls = tool_calls.len(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "response out"
    );

    // Persist the resume point so a restart resumes the same remote session.
    if let Some(mid) = message_id {
        if let Some(t) = state.threads.lock().unwrap().get_mut(&key) {
            t.parent_message_id = Some(mid);
        }
        let _ = state.store.save_thread(
            &key,
            &store::ThreadResume {
                session_id: thread.session_id.clone(),
                parent_message_id: Some(mid.to_string()),
            },
        );
    }

    // Tool calls live inside the answer text as <invoke ...> blocks.
    let visible = if tool_calls.is_empty() {
        answer.clone()
    } else {
        dialect::strip_tool_calls(&answer).trim().to_string()
    };

    let mut message = json!({
        "role": "assistant",
        "content": visible,
    });
    if !reasoning.is_empty() {
        message["reasoning_content"] = json!(reasoning);
    }
    if !tool_calls.is_empty() {
        message["tool_calls"] = json!(tool_calls
            .iter()
            .enumerate()
            .map(|(i, c)| json!({
                "id": format!("call_{i}"),
                "type": "function",
                "function": {
                    "name": c.name,
                    "arguments": c.arguments.to_string(),
                }
            }))
            .collect::<Vec<_>>());
    }

    Json(json!({
        "id": id,
        "object": "chat.completion",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": prompt.len().div_ceil(4),
            "completion_tokens": answer.len().div_ceil(4),
            "total_tokens": (prompt.len() + answer.len()).div_ceil(4)
        }
    }))
    .into_response()
}

/// Result of draining one upstream SSE response into OpenAI chunks.
struct Pumped {
    parent: Option<i64>,
    filter: dialect::StreamToolFilter,
    /// True once any chunk reached the client; a hint after that is fatal.
    emitted: bool,
    /// Fatal `event: hint` payload, if the upstream aborted the stream.
    hint: Option<String>,
}

/// Read one upstream stream to completion, forwarding chunks to `tx`.
async fn pump(
    resp: reqwest::Response,
    tx: &tokio::sync::mpsc::Sender<Result<Event, Infallible>>,
    id: &str,
    model: &str,
    created: u64,
) -> Pumped {
    let mut router = FragmentRouter::new();
    let mut events = Vec::new();
    let mut buf = String::new();
    let mut current_event = String::new();
    let mut parent: Option<i64> = None;
    let mut filter = dialect::StreamToolFilter::new();
    let mut emitted = false;

    let mut byte_stream = resp.bytes_stream();
    while let Some(chunk) = byte_stream.next().await {
        let Ok(bytes) = chunk else { break };
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
                    };
                }
                protocol::LineKind::Event(value) => value,
            };
            if parent.is_none() {
                if let Some(mid) = value.get("response_message_id").and_then(|v| v.as_i64()) {
                    parent = Some(mid);
                }
            }
            events.clear();
            route_event(&value, &mut router, &mut events);
            for ev in &events {
                let delta = match ev.channel {
                    Channel::Text => {
                        // Tool blocks are held back and surfaced as tool_calls.
                        let safe = filter.feed(&ev.text);
                        if safe.is_empty() {
                            continue;
                        }
                        json!({ "content": safe })
                    }
                    Channel::Reasoning => json!({ "reasoning_content": ev.text }),
                };
                let chunk = json!({
                    "id": id,
                    "object": "chat.completion.chunk",
                    "created": created,
                    "model": model,
                    "choices": [{"index": 0, "delta": delta, "finish_reason": null}]
                });
                emitted = true;
                if tx.send(Ok(Event::default().data(chunk.to_string()))).await.is_err() {
                    return Pumped {
                        parent,
                        filter,
                        emitted,
                        hint: None,
                    };
                }
            }
        }
    }

    Pumped {
        parent,
        filter,
        emitted,
        hint: None,
    }
}

#[allow(clippy::too_many_arguments)]
async fn stream_response(
    state: AppState,
    key: String,
    resp: reqwest::Response,
    body: CompletionBody,
    id: String,
    model: String,
    created: u64,
    replay: String,
    can_recover: bool,
    started: std::time::Instant,
) -> Response {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(32);

    tokio::spawn(async move {
        let mut pumped = pump(resp, &tx, &id, &model, created).await;

        // Context overflow arrives as a hint before any content. Rebuild the
        // remote session and replay the transcript once, like the Python proxy.
        if let Some(hint) = pumped.hint.clone() {
            if can_recover && !pumped.emitted && protocol::is_context_limit(&hint) {
                if let Ok(fresh) = rebuild_session(&state, &key, &hint).await {
                    let retry_body = completion_body(
                        &fresh,
                        &replay,
                        body.thinking_enabled,
                        body.search_enabled,
                    );
                    match completion_with_retry(&state, &key, &retry_body).await {
                        Ok(resp) => pumped = pump(resp, &tx, &id, &model, created).await,
                        Err(e) => tracing::error!("thread {key}: replay failed: {e}"),
                    }
                }
            } else {
                tracing::error!("thread {key}: upstream hint error: {hint}");
            }
        }

        let Pumped {
            parent,
            mut filter,
            ..
        } = pumped;

        // Flush any held-back prose, then emit tool calls (if any).
        let tail = filter.finish();
        if !tail.is_empty() {
            let chunk = json!({
                "id": id, "object": "chat.completion.chunk",
                "created": created, "model": model,
                "choices": [{"index": 0, "delta": {"content": tail}, "finish_reason": null}]
            });
            let _ = tx.send(Ok(Event::default().data(chunk.to_string()))).await;
        }

        let finish_reason = if filter.calls.is_empty() {
            "stop"
        } else {
            let calls: Vec<Value> = filter
                .calls
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    json!({
                        "index": i,
                        "id": format!("call_{i}"),
                        "type": "function",
                        "function": { "name": c.name, "arguments": c.arguments.to_string() }
                    })
                })
                .collect();
            let chunk = json!({
                "id": id, "object": "chat.completion.chunk",
                "created": created, "model": model,
                "choices": [{"index": 0, "delta": {"tool_calls": calls}, "finish_reason": null}]
            });
            let _ = tx.send(Ok(Event::default().data(chunk.to_string()))).await;
            "tool_calls"
        };

        state
            .stats
            .tool_calls
            .fetch_add(filter.calls.len() as u64, Ordering::Relaxed);
        state.stats.ok.fetch_add(1, Ordering::Relaxed);
        tracing::info!(
            thread = %key,
            finish_reason,
            tool_calls = filter.calls.len(),
            parent = ?parent,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "stream done"
        );

        let done = json!({
            "id": id,
            "object": "chat.completion.chunk",
            "created": created,
            "model": model,
            "choices": [{"index": 0, "delta": {}, "finish_reason": finish_reason}]
        });
        let _ = tx.send(Ok(Event::default().data(done.to_string()))).await;
        let _ = tx.send(Ok(Event::default().data("[DONE]"))).await;

        if let Some(mid) = parent {
            if let Some(t) = state.threads.lock().unwrap().get_mut(&key) {
                t.parent_message_id = Some(mid);
            }
            let session_id = state
                .threads
                .lock()
                .unwrap()
                .get(&key)
                .map(|t| t.session_id.clone());
            if let Some(session_id) = session_id {
                let _ = state.store.save_thread(
                    &key,
                    &store::ThreadResume {
                        session_id,
                        parent_message_id: Some(mid.to_string()),
                    },
                );
            }
        }
    });

    Sse::new(ReceiverStream::new(rx)).into_response()
}

fn route_all(raw: &str) -> (String, String, Option<i64>) {
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
        .find_map(|v| v.get("response_message_id").and_then(|x| x.as_i64()));
    (reasoning, answer, mid)
}

fn uuid_like() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{n:x}")
}

fn unix_secs() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn bad_request(msg: &str) -> Response {
    (
        axum::http::StatusCode::BAD_REQUEST,
        Json(json!({"error": {"message": msg, "type": "invalid_request_error"}})),
    )
        .into_response()
}

fn backend_error(msg: &str) -> Response {
    (
        axum::http::StatusCode::BAD_GATEWAY,
        Json(json!({"error": {"message": msg, "type": "backend_error"}})),
    )
        .into_response()
}


/// Build a connected account pool from resolved config.
pub(crate) async fn connect_pool(
    cfg: &config::Config,
    store: &store::Store,
    accounts: Vec<pool::AccountSpec>,
) -> Result<Arc<pool::Pool>, Box<dyn std::error::Error>> {
    let pool_config = pool::PoolConfig {
        request_delay: std::time::Duration::from_secs_f64(cfg.request_delay_secs),
        queue_limit: cfg.queue_limit,
        login_cooldown: std::time::Duration::from_secs(cfg.login_cooldown_secs),
    };
    let p = pool::Pool::connect(accounts, pool_config, store.clone()).await?;
    Ok(Arc::new(p))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Rust ignores SIGPIPE by default, so `deepseek-rs stats | head` panics with
    // "failed printing to stdout: Broken pipe". Restore the usual Unix
    // behaviour: die quietly when the reader goes away.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(String::as_str).unwrap_or("serve");

    // `serve` is the only long-running command; everything else is short-lived
    // and lives in `cli`.
    if cmd != "serve" {
        let code = cli::run(&args).await?;
        std::process::exit(code);
    }

    let profile = log::Profile::from_env();
    log::init(profile);

    // Claim the pidfile before doing any work, so a second `serve` fails fast
    // instead of fighting over the DB lock.
    let _pid_guard = cli::PidGuard::claim()?;

    let cfg = config::Config::load()?;
    let store = store::Store::open_path(&cfg.db_path)?;

    let accounts = cli::load_accounts(&store);
    if accounts.is_empty() {
        return Err(
            "no DeepSeek accounts configured — add one with `deepseek-rs accounts add <email> <password>`"
                .into(),
        );
    }

    tracing::info!(
        profile = ?profile,
        accounts = accounts.len(),
        request_delay_secs = cfg.request_delay_secs,
        "starting deepseek-rs"
    );
    tracing::info!(
        "connecting {} account(s), {}s between requests",
        accounts.len(),
        cfg.request_delay_secs
    );
    let pool = connect_pool(&cfg, &store, accounts).await?;
    tracing::info!("login ok for {} account(s)", pool.len());

    // Fail fast on a broken PoW path rather than on the first user request.
    {
        let conn = pool.any();
        let client = conn.client().await;
        client.pow_challenge(COMPLETION_PATH).await?;
        tracing::info!("pow challenge ok (account {})", conn.email);
    }

    let settings = Settings {
        thinking_default: cfg.thinking,
        search_enabled: cfg.search,
        model_name: cfg.model.clone(),
        tool_reminder_interval: cfg.tool_reminder_interval,
    };
    tracing::info!(
        db_path = %cfg.db_path,
        model = %settings.model_name,
        thinking = settings.thinking_default,
        search = settings.search_enabled,
        "settings loaded"
    );

    let state = AppState {
        pool,
        settings,
        threads: Arc::new(Mutex::new(HashMap::new())),
        store,
        stats: Arc::new(Stats::new()),
    };

    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/stats", get(stats))
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(chat_completions))
        .with_state(state);

    let addr = format!("{}:{}", cfg.host, cfg.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("listening on http://{addr}");

    // Graceful shutdown: finish in-flight requests, drop the pidfile.
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    tracing::info!("stopped");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    let term = async {
        use tokio::signal::unix::{signal, SignalKind};
        if let Ok(mut s) = signal(SignalKind::terminate()) {
            s.recv().await;
        }
    };
    tokio::select! {
        _ = ctrl_c => tracing::info!("SIGINT received, shutting down"),
        _ = term => tracing::info!("SIGTERM received, shutting down"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: &str) -> Msg {
        Msg {
            role: role.into(),
            content: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
            name: None,
        }
    }

    fn thread(exchanges: usize) -> ThreadState {
        ThreadState {
            session_id: "s".into(),
            parent_message_id: None,
            exchanges,
        }
    }

    #[test]
    fn counts_only_user_and_tool() {
        // Fresh thread: no assistant yet, so the whole request is the delta.
        let msgs = [msg("system"), msg("user"), msg("tool")];
        assert_eq!(counted_messages(&msgs), 2);
    }

    #[test]
    fn counts_only_the_delta_after_the_last_assistant() {
        // pi resends everything; only the tail counts toward the interval.
        let msgs = [
            msg("user"),
            msg("assistant"),
            msg("tool"),
            msg("assistant"),
            msg("user"),
        ];
        assert_eq!(counted_messages(&msgs), 1);
    }

    #[test]
    fn interval_zero_never_fires() {
        let msgs = [msg("user")];
        assert!(!reminder_due(Some(&thread(100)), &msgs, 0));
        assert!(!reminder_due(None, &msgs, 0));
    }

    #[test]
    fn fires_on_boundary_crossing_only() {
        let msgs = [msg("user")];
        // 7 -> 8 crosses the interval of 8.
        assert!(reminder_due(Some(&thread(7)), &msgs, 8));
        // 8 -> 9 stays inside the same interval.
        assert!(!reminder_due(Some(&thread(8)), &msgs, 8));
        // 15 -> 16 crosses again.
        assert!(reminder_due(Some(&thread(15)), &msgs, 8));
    }

    #[test]
    fn fresh_thread_fires_on_first_turn() {
        // A new thread starts at zero, so 0 -> 1 is inside interval 8.
        assert!(!reminder_due(None, &[msg("user")], 8));
        // But a turn carrying enough messages can cross the first boundary.
        let many: Vec<Msg> = (0..8).map(|_| msg("tool")).collect();
        assert!(reminder_due(None, &many, 8));
    }

    #[test]
    fn reminder_text_names_the_failure_modes() {
        let text = dialect::periodic_reminder();
        println!("-----BEGIN-----\n{text}\n-----END-----");
        assert!(text.contains("DROPPED"));
        assert!(text.contains("closing tag twice"));
        assert!(text.contains("one JSON object"));
        assert!(text.contains(dialect::TEMPLATE));
    }
}
