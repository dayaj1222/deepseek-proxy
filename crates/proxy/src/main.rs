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

mod dialect;
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
use client::{Client, CompletionBody, COMPLETION_PATH};
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
    client: Client,
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
    body: &CompletionBody,
) -> Result<reqwest::Response, String> {
    const ATTEMPTS: u32 = 3;
    let mut last = String::new();

    for attempt in 0..ATTEMPTS {
        match state.client.stream_completion(body).await {
            Ok(resp) => return Ok(resp),
            Err(e) => {
                last = e.to_string();
                let retryable = last.contains("rate_limit")
                    || last.contains("too frequent")
                    || last.contains("generation_err")
                    || last.contains("temporarily unavailable");
                if !retryable || attempt + 1 == ATTEMPTS {
                    break;
                }
                let wait_ms = 500u64 * (1 << attempt);
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

    let prompt = match &existing {
        Some(_) => match newest_user_message(&req.messages) {
            Some(p) => p,
            None => return bad_request("no user message in continuation turn"),
        },
        None => {
            let transcript = flatten_transcript(&req.messages);
            if system_extra.is_empty() {
                transcript
            } else {
                format!("{transcript}\n\n{system_extra}")
            }
        }
    };
    if prompt.is_empty() {
        return bad_request("empty prompt");
    }

    // Create the remote session lazily, on the first turn for this thread.
    let thread = match existing {
        Some(t) => t,
        None => {
            let session_id = match state.client.create_chat_session().await {
                Ok(s) => s,
                Err(e) => return backend_error(&format!("create_session: {e}")),
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
    let mut body = CompletionBody::new(&thread.session_id, &prompt);
    body.thinking_enabled = thinking;
    body.search_enabled = state.settings.search_enabled;
    body.parent_message_id = thread.parent_message_id;

    let model = req.model.clone().unwrap_or_else(|| state.settings.model_name.clone());
    let id = format!("chatcmpl-{}", uuid_like());
    let created = unix_secs();

    let resp = match completion_with_retry(&state, &body).await {
        Ok(r) => r,
        Err(e) => return backend_error(&format!("completion: {e}")),
    };

    if req.stream {
        return stream_response(state, key, resp, id, model, created).await;
    }

    // Non-stream: buffer everything, then answer in one shot.
    let raw = match resp.text().await {
        Ok(t) => t,
        Err(e) => return backend_error(&format!("read body: {e}")),
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

async fn stream_response(
    state: AppState,
    key: String,
    resp: reqwest::Response,
    id: String,
    model: String,
    created: u64,
) -> Response {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(32);

    tokio::spawn(async move {
        let mut router = FragmentRouter::new();
        let mut events = Vec::new();
        let mut buf = String::new();
        let mut parent: Option<i64> = None;

        let mut byte_stream = resp.bytes_stream();
        while let Some(chunk) = byte_stream.next().await {
            let Ok(bytes) = chunk else { break };
            buf.push_str(&String::from_utf8_lossy(&bytes));
            while let Some(pos) = buf.find('\n') {
                let line = buf[..pos].trim().to_string();
                buf.drain(..=pos);
                let Some(payload) = line.strip_prefix("data:") else {
                    continue;
                };
                let payload = payload.trim();
                if payload.is_empty() || payload == "[DONE]" {
                    continue;
                }
                let Ok(value) = serde_json::from_str::<Value>(payload) else {
                    continue;
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
                        Channel::Text => json!({ "content": ev.text }),
                        Channel::Reasoning => json!({ "reasoning_content": ev.text }),
                    };
                    let chunk = json!({
                        "id": id,
                        "object": "chat.completion.chunk",
                        "created": created,
                        "model": model,
                        "choices": [{"index": 0, "delta": delta, "finish_reason": null}]
                    });
                    if tx.send(Ok(Event::default().data(chunk.to_string()))).await.is_err() {
                        return;
                    }
                }
            }
        }

        let done = json!({
            "id": id,
            "object": "chat.completion.chunk",
            "created": created,
            "model": model,
            "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]
        });
        let _ = tx.send(Ok(Event::default().data(done.to_string()))).await;
        let _ = tx.send(Ok(Event::default().data("[DONE]"))).await;

        if let Some(mid) = parent {
            if let Some(t) = state.threads.lock().unwrap().get_mut(&key) {
                t.parent_message_id = Some(mid);
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

fn env_flag(key: &str, default: bool) -> bool {
    match std::env::var(key) {
        Ok(v) => matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"),
        Err(_) => default,
    }
}

fn env_from_dotenv(key: &str) -> Option<String> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../deepseek/.env");
    let text = std::fs::read_to_string(path).ok()?;
    text.lines()
        .filter_map(|l| l.split_once('='))
        .find(|(k, _)| k.trim() == key)
        .map(|(_, v)| v.trim().trim_matches('"').to_string())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // `deepseek-rs key <name>` registers an API key in the store and exits.
    // Reading the key from argv keeps this scriptable; it is never logged.
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("key") {
        let name = args.get(2).map(String::as_str).unwrap_or("default");
        let key = args
            .get(3)
            .cloned()
            .ok_or("usage: deepseek-rs key <name> <api-key>")?;
        let db_path = std::env::var("DB_PATH").unwrap_or_else(|_| "deepseek-rs.db".into());
        let store = store::Store::open_path(&db_path)?;
        store.insert_api_key(name, &key)?;
        println!("registered key '{name}' in {db_path}");
        return Ok(());
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "proxy=info,tower_http=warn".into()),
        )
        .init();

    let email = std::env::var("DEEPSEEK_EMAIL_0")
        .ok()
        .or_else(|| env_from_dotenv("DEEPSEEK_EMAIL_0"))
        .ok_or("DEEPSEEK_EMAIL_0 not set (env or ../deepseek/.env)")?;
    let password = std::env::var("DEEPSEEK_PASSWORD_0")
        .ok()
        .or_else(|| env_from_dotenv("DEEPSEEK_PASSWORD_0"))
        .ok_or("DEEPSEEK_PASSWORD_0 not set (env or ../deepseek/.env)")?;

    tracing::info!("logging in to DeepSeek");
    let client = Client::login(&email, &password, client::get_device_id()).await?;
    tracing::info!("login ok (token {}...)", &client.token()[..12.min(client.token().len())]);

    // Fail fast if the PoW path is broken, rather than on the first user request.
    let _ = client.pow_challenge(COMPLETION_PATH).await?;
    tracing::info!("pow challenge ok");

    let settings = Settings {
        thinking_default: env_flag("THINKING_ENABLED", false),
        search_enabled: env_flag("SEARCH_ENABLED", false),
        model_name: std::env::var("MODEL_NAME").unwrap_or_else(|_| "deepseek-chat".into()),
    };
    tracing::info!(
        "thinking={} search={}",
        settings.thinking_default,
        settings.search_enabled
    );

    let db_path = std::env::var("DB_PATH").unwrap_or_else(|_| "deepseek-rs.db".into());
    let store = store::Store::open_path(&db_path)?;
    tracing::info!("store: {db_path}");

    let state = AppState {
        client,
        settings,
        threads: Arc::new(Mutex::new(HashMap::new())),
        store,
    };

    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(chat_completions))
        .with_state(state);

    let host = std::env::var("PROXY_HOST").unwrap_or_else(|_| "0.0.0.0".into());
    let port = std::env::var("PROXY_PORT").unwrap_or_else(|_| "9174".into());
    let addr = format!("{host}:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("listening on http://{addr}");
    axum::serve(listener, app).await?;
    Ok(())
}
