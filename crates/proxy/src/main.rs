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
//! `(chat_session_id, parent_message_id)`. Threads are keyed by the
//! `x-thread-id` header when present, otherwise by a hash of the first system
//! and user message. Continuation turns send only the newest user message,
//! matching the Python proxy's delta behaviour; a fresh thread sends the
//! flattened transcript.
//!
//! ## Not yet ported
//!
//! - SQLite persistence of threads (in-memory only; lost on restart)
//! - `tools/format.py` prompt + tool-call dialect injection
//! - multi-account failover

mod config;
mod dialect;
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
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};
use tokio_stream::wrappers::ReceiverStream;

#[derive(Clone)]
struct Settings {
    thinking_default: bool,
    search_enabled: bool,
    model_name: String,
}

#[derive(Debug, Clone)]
struct ThreadState {
    session_id: String,
    parent_message_id: Option<i64>,
}

#[derive(Clone)]
struct AppState {
    pool: Arc<pool::Pool>,
    settings: Settings,
    threads: Arc<Mutex<HashMap<String, ThreadState>>>,
    store: store::Store,
}

#[derive(Debug, Deserialize)]
struct ChatRequest {
    #[serde(default)]
    model: Option<String>,
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
) -> (String, String) {
    let transcript = flatten_transcript(&req.messages);
    let replay = if system_extra.is_empty() {
        transcript
    } else {
        format!("{transcript}\n\n{system_extra}")
    };
    let delta = match existing {
        Some(_) => continuation_prompt(&req.messages).unwrap_or_default(),
        None => replay.clone(),
    };
    (delta, replay)
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

fn thread_key(headers: &HeaderMap, req: &ChatRequest) -> String {
    if let Some(v) = headers.get("x-thread-id").and_then(|v| v.to_str().ok()) {
        if !v.trim().is_empty() {
            return v.trim().to_string();
        }
    }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for m in &req.messages {
        if m.role == "system" || m.role == "user" {
            m.role.hash(&mut h);
            content_text(&m.content).hash(&mut h);
        }
    }
    format!("auto-{:x}", h.finish())
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
        return resp;
    }

    let key = thread_key(&headers, &req);

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
            })
        });

    // Tool definitions are taught once, on the first turn of a thread. Later
    // turns ride on the server-side conversation, like the Python proxy.
    let system_extra = dialect::inject_tool_descriptions("", &req.tools);

    let (prompt, replay) = build_prompts(&req, existing.as_ref(), &system_extra);
    if prompt.is_empty() {
        return bad_request("no user message or tool result in continuation turn");
    }
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
            };
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

    let model = req.model.clone().unwrap_or_else(|| state.settings.model_name.clone());
    let id = format!("chatcmpl-{}", uuid_like());
    let created = unix_secs();

    let resp = match completion_with_retry(&state, &key, &body).await {
        Ok(r) => r,
        Err(e) => return backend_error(&format!("completion: {e}")),
    };

    if req.stream {
        return stream_response(state, key, resp, body, id, model, created, replay, can_recover)
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
    let tool_calls = dialect::extract_tool_calls(&answer);
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

fn env_from_dotenv(key: &str) -> Option<String> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../deepseek/.env");
    let text = std::fs::read_to_string(path).ok()?;
    text.lines()
        .filter_map(|l| l.split_once('='))
        .find(|(k, _)| k.trim() == key)
        .map(|(_, v)| v.trim().trim_matches('"').to_string())
}

/// Load accounts from DEEPSEEK_EMAIL_<i>/PASSWORD_<i>/TOKEN_<i>, falling back to
/// an unindexed EMAIL/PASSWORD/TOKEN, then to ../deepseek/.env.
fn load_accounts() -> Vec<pool::AccountSpec> {
    fn val(key: &str) -> Option<String> {
        std::env::var(key)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .or_else(|| env_from_dotenv(key).filter(|v| !v.trim().is_empty()))
    }

    let mut out = Vec::new();
    for i in 0.. {
        let email = val(&format!("DEEPSEEK_EMAIL_{i}"));
        let password = val(&format!("DEEPSEEK_PASSWORD_{i}"));
        let token = val(&format!("DEEPSEEK_TOKEN_{i}"));
        if token.is_none() && !(email.is_some() && password.is_some()) {
            break;
        }
        out.push(pool::AccountSpec {
            email: email.unwrap_or_else(|| format!("token-account-{i}")),
            password: password.unwrap_or_default(),
            token,
        });
    }

    if out.is_empty() {
        let email = val("DEEPSEEK_EMAIL");
        let password = val("DEEPSEEK_PASSWORD");
        let token = val("DEEPSEEK_TOKEN");
        if token.is_some() || (email.is_some() && password.is_some()) {
            out.push(pool::AccountSpec {
                email: email.unwrap_or_else(|| "token-account".into()),
                password: password.unwrap_or_default(),
                token,
            });
        }
    }
    out
}

fn db_path() -> Result<String, Box<dyn std::error::Error>> {
    Ok(config::Config::load()?.db_path)
}

/// `deepseek-rs key <name> <key>` — register an API key and exit.
fn cmd_key(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let name = args.get(2).map(String::as_str).unwrap_or("default");
    let key = args
        .get(3)
        .cloned()
        .ok_or("usage: deepseek-rs key <name> <api-key>")?;
    let path = db_path()?;
    let store = store::Store::open_path(&path)?;
    store.insert_api_key(name, &key)?;
    println!("registered key '{name}' in {path}");
    Ok(())
}

fn usage() {
    println!(
        "deepseek-rs — OpenAI-compatible proxy for the DeepSeek private API

Usage:
  deepseek-rs serve                      run the proxy (foreground)
  deepseek-rs key <name> <api-key>       register an API key
  deepseek-rs accounts                   list configured accounts
  deepseek-rs check                      verify login + PoW, then exit
  deepseek-rs url                        print the base URL
  deepseek-rs help                       this message

Environment:
  PROXY_HOST / PROXY_PORT   bind address (default 0.0.0.0:9174)
  DB_PATH                   sqlite file (default deepseek-rs.db)
  REQUEST_DELAY             seconds between requests per account (default 2)
  THINKING_ENABLED          default thinking flag (default false)
  SEARCH_ENABLED            send search flag (default false)
  DEEPSEEK_EMAIL_<i> / DEEPSEEK_PASSWORD_<i> / DEEPSEEK_TOKEN_<i>
                            accounts, checked in order"
    );
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(String::as_str).unwrap_or("serve");

    match cmd {
        "key" => return cmd_key(&args),
        "help" | "-h" | "--help" => {
            usage();
            return Ok(());
        }
        "url" => {
            let cfg = config::Config::load()?;
            println!("http://127.0.0.1:{}/v1", cfg.port);
            return Ok(());
        }
        _ => {}
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "proxy=info".into()),
        )
        .init();

    let accounts = load_accounts();
    if accounts.is_empty() {
        return Err("no DeepSeek accounts configured (DEEPSEEK_EMAIL_0/PASSWORD_0)".into());
    }

    if cmd == "accounts" {
        for (i, a) in accounts.iter().enumerate() {
            let kind = if a.token.is_some() { "token" } else { "password" };
            println!("{i}: {} ({kind})", a.email);
        }
        return Ok(());
    }

    let store = store::Store::open_path(db_path()?)?;

    let cfg = match config::Config::load() {
        Ok(c) => c,
        Err(e) => return Err(e.into()),
    };
    let delay = cfg.request_delay_secs;
    let pool_config = pool::PoolConfig {
        request_delay: std::time::Duration::from_secs_f64(delay),
        queue_limit: cfg.queue_limit,
        login_cooldown: std::time::Duration::from_secs(cfg.login_cooldown_secs),
    };

    tracing::info!("connecting {} account(s), {delay}s between requests", accounts.len());
    let pool = match pool::Pool::connect(accounts, pool_config, store.clone()).await {
        Ok(p) => Arc::new(p),
        Err(e) => return Err(e.into()),
    };
    tracing::info!("login ok for {} account(s)", pool.len());

    // Fail fast on a broken PoW path rather than on the first user request.
    {
        let conn = pool.route("startup-check").await;
        let client = conn.client().await;
        client.pow_challenge(COMPLETION_PATH).await?;
        tracing::info!("pow challenge ok (account {})", conn.email);
    }

    if cmd == "check" {
        println!("ok: {} account(s), login + pow verified", pool.len());
        return Ok(());
    }

    let settings = Settings {
        thinking_default: cfg.thinking,
        search_enabled: cfg.search,
        model_name: cfg.model.clone(),
    };
    tracing::info!(
        "thinking={} search={}",
        settings.thinking_default,
        settings.search_enabled
    );

    let state = AppState {
        pool,
        settings,
        threads: Arc::new(Mutex::new(HashMap::new())),
        store,
    };

    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(chat_completions))
        .with_state(state);

    let addr = format!("{}:{}", cfg.host, cfg.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("listening on http://{addr}");
    axum::serve(listener, app).await?;
    Ok(())
}
