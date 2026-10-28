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
mod dialect_dsml;
mod dialect_simple;
mod images;
mod log;
mod pool;
mod prompt;
mod session;
mod sse;
mod store;
mod threads;
mod turn;

use axum::{
    extract::State,
    http::HeaderMap,
    response::{
        sse::Sse,
        IntoResponse, Response,
    },
    routing::{get, post},
    Json, Router,
};
use client::{CompletionBody, COMPLETION_PATH};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sse::{pump, route_all, unix_secs, usage_block, uuid_like, Pumped};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use threads::ThreadState;
use tracing::Instrument;

#[derive(Clone)]
struct Settings {
    thinking_default: bool,
    search_enabled: bool,
    model_name: String,
    /// Throttle retries before the first byte (`RATE_LIMIT_MAX_RETRIES`).
    rate_limit_max_retries: u32,
    /// Base backoff (`RATE_LIMIT_BACKOFF_S`), floored at 0.5s like Python.
    rate_limit_backoff_secs: f64,
    /// Correction turns for malformed tool calls (`TOOL_REPAIR_MAX_RETRIES`).
    tool_repair_max_retries: u32,
    /// Whole-turn deadline in seconds (`REQUEST_TIMEOUT`).
    request_timeout_secs: f64,
    /// Largest accepted image in bytes (`IMAGE_MAX_BYTES`).
    image_max_bytes: usize,
    /// SSE keep-alive interval in seconds (`HEARTBEAT_INTERVAL`).
    heartbeat_interval_secs: f64,
    /// Wire dialect selected at startup (`dialect` / `DIALECT`).
    dialect: dialect::Dialect,
}

impl Settings {
    /// Project the reloadable subset of the resolved config.
    ///
    /// Restart-only keys (`host`, `port`, `db_path`, `image_max_bytes`) are
    /// still copied here because a running process reads them from somewhere,
    /// but a reload cannot change them; see [`restart_only_keys`].
    fn from_config(cfg: &config::Config) -> Self {
        Self {
            thinking_default: cfg.thinking,
            search_enabled: cfg.search,
            model_name: cfg.model.clone(),
            rate_limit_max_retries: cfg.rate_limit_max_retries,
            rate_limit_backoff_secs: cfg.rate_limit_backoff_secs,
            tool_repair_max_retries: cfg.tool_repair_max_retries,
            request_timeout_secs: cfg.request_timeout_secs,
            image_max_bytes: cfg.image_max_bytes,
            heartbeat_interval_secs: cfg.heartbeat_interval_secs,
            dialect: cfg.dialect,
        }
    }
}

#[derive(Clone)]
struct AppState {
    /// Live knobs. Behind a handle so a SIGHUP can swap them without a restart
    /// or a lock held across an await.
    settings: Arc<SettingsHandle>,
    /// Account pool. Swappable so a reload can add or remove an account without
    /// restarting; callers go through [`AppState::pool`] and never hold the lock
    /// across an await, so an in-flight turn finishes on the pool it started on.
    pool: Arc<std::sync::RwLock<Arc<pool::Pool>>>,
    /// Cache, turn lock and persistence for conversation threads.
    threads: Arc<threads::Threads>,
    store: store::Store,
    stats: Arc<Stats>,
}

impl AppState {
    /// Snapshot the current pool. One Arc clone; safe to hold for a whole turn.
    fn pool(&self) -> Arc<pool::Pool> {
        self.pool
            .read()
            .expect("pool lock poisoned")
            .clone()
    }

    /// Install a rebuilt pool. Turns already running keep their snapshot.
    fn set_pool(&self, fresh: Arc<pool::Pool>) {
        *self.pool.write().expect("pool lock poisoned") = fresh;
    }
}

/// The current `Settings`, swappable at runtime.
///
/// A turn takes one [`get`](Self::get) snapshot at its start and uses it for its
/// whole life: changing a turn's deadline halfway through a stream would be a
/// bug, not a feature. New turns see new values; running turns finish on the
/// values they started with.
struct SettingsHandle {
    current: std::sync::RwLock<Arc<Settings>>,
    /// The resolved config behind `current`, kept so a reload can tell a
    /// restart-only key apart from a live one.
    config: std::sync::Mutex<config::Config>,
    /// Bumped on every successful reload; 1 means "as loaded at boot".
    generation: AtomicU64,
    /// When the running config was installed.
    loaded_at: std::sync::Mutex<Instant>,
    /// Last reload failure. `None` means the running config is the last good
    /// one and nothing has failed since.
    last_error: std::sync::Mutex<Option<String>>,
}

/// Keys that cannot change while the process is running.
///
/// `host`/`port` are already bound, `db_path` is an open SQLite handle, and
/// `image_max_bytes` is baked into `DefaultBodyLimit` when the router is built.
/// A reload reports these instead of pretending to apply them.
fn restart_only_keys(old: &config::Config, new: &config::Config) -> Vec<&'static str> {
    let mut changed = Vec::new();
    if old.host != new.host || old.port != new.port {
        changed.push("host/port");
    }
    if old.db_path != new.db_path {
        changed.push("db_path");
    }
    if old.image_max_bytes != new.image_max_bytes {
        changed.push("image_max_bytes");
    }
    if old.dialect != new.dialect {
        changed.push("dialect");
    }
    changed
}

impl SettingsHandle {
    fn new(config: config::Config) -> Self {
        Self {
            current: std::sync::RwLock::new(Arc::new(Settings::from_config(&config))),
            config: std::sync::Mutex::new(config),
            generation: AtomicU64::new(1),
            loaded_at: std::sync::Mutex::new(Instant::now()),
            last_error: std::sync::Mutex::new(None),
        }
    }

    /// Install a new config. Running turns keep the snapshot they took.
    fn replace(&self, config: config::Config) -> u64 {
        let settings = Settings::from_config(&config);
        *self.current.write().expect("settings lock poisoned") = Arc::new(settings);
        *self.config.lock().expect("settings lock poisoned") = config;
        *self.loaded_at.lock().expect("settings lock poisoned") = Instant::now();
        *self.last_error.lock().expect("settings lock poisoned") = None;
        self.generation.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Record a failed reload. The running config stays untouched.
    fn note_error(&self, error: String) {
        *self.last_error.lock().expect("settings lock poisoned") = Some(error);
    }

    /// `(generation, seconds_since_last_successful_load, last_error)`.
    fn snapshot(&self) -> (u64, u64, Option<String>) {
        let last_error = self.last_error.lock().expect("settings lock poisoned").clone();
        (
            self.generation.load(Ordering::Relaxed),
            self.loaded_at
                .lock()
                .expect("settings lock poisoned")
                .elapsed()
                .as_secs(),
            last_error,
        )
    }

    /// Restart-only keys that changed since the running config was loaded.
    fn restart_only_changes(&self, new: &config::Config) -> Vec<&'static str> {
        let old = self.config.lock().expect("settings lock poisoned");
        restart_only_keys(&old, new)
    }

    /// Cheap: one Arc clone under a read lock, never held across an await.
    fn get(&self) -> Arc<Settings> {
        self.current
            .read()
            .expect("settings lock poisoned")
            .clone()
    }
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
    /// Images dropped because a turn carried more than DeepSeek accepts.
    images_dropped: AtomicU64,
    /// Set when SIGTERM/SIGINT arrives; `/readyz` reports 503 from then on.
    shutting_down: std::sync::atomic::AtomicBool,
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
    /// OpenAI `tool_choice`: "none" drops every tool, "required" forces a call,
    /// an object narrows to one named function.
    #[serde(default)]
    tool_choice: Option<Value>,
    /// OpenAI `parallel_tool_calls`; defaults to true like the OpenAI API.
    #[serde(default = "default_true")]
    parallel_tool_calls: bool,
    /// OpenAI `stream_options`; only `include_usage` is honoured.
    #[serde(default)]
    stream_options: Option<Value>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize)]
struct Msg {
    role: String,
    #[serde(default)]
    content: Option<Value>,
    // Deserialized for completeness; the prompt builders only need role/content.
    #[serde(default)]

    tool_calls: Vec<Value>,
    /// Tool-call id this result answers, rendered as `Tool result (id=...)`.
    #[serde(default)]
    tool_call_id: Option<String>,
}

/// Prompts for one turn: the delta the session needs now, and the full
/// flattened transcript used to rebuild the session when it overflows.
///
/// Mirrors the Python proxy's `prompt` / `replay_prompt` pair.

#[cfg(test)]
fn build_prompts(
    req: &ChatRequest,
    existing: Option<&ThreadState>,
    system_extra: &str,
    image_note: Option<&str>,
) -> (String, String) {
    build_prompts_with(
        req,
        existing,
        system_extra,
        image_note,
        dialect::Dialect::JsonInvoke,
    )
}

/// Dialect-aware prompt builder. `json_invoke` keeps the original path
/// byte-for-byte; the DSML dialect owns its whole transcript shape (BOS,
/// system prefix, folded user turns, generation tail).
fn build_prompts_with(
    req: &ChatRequest,
    existing: Option<&ThreadState>,
    system_extra: &str,
    image_note: Option<&str>,
    wire: dialect::Dialect,
) -> (String, String) {
    let (specs, required) = resolve_tools(req);
    let reminder = prompt::reminder(&req.tools, wire);
    let directive = prompt::directive(
        !specs.is_empty(),
        required,
        req.parallel_tool_calls,
        prompt::choice_none(req.tool_choice.as_ref()),
    );
    let turn = prompt::Turn {
        messages: &req.messages,
        continuation: existing.is_some(),
        system_extra,
        image_note,
        reminder: reminder.as_deref(),
        directive,
    };
    let out = match wire {
        dialect::Dialect::JsonInvoke => prompt::for_turn(&turn),
        other => prompt::for_turn_with_dialect(&turn, other),
    };
    (out.delta, out.replay)
}

fn completion_body(
    thread: &ThreadState,
    prompt: &str,
    thinking: bool,
    search: bool,
    ref_file_ids: &[String],
    model: &str,
) -> CompletionBody {
    let mut body = CompletionBody::new(&thread.session_id, prompt);
    body.thinking_enabled = thinking;
    body.search_enabled = search;
    body.parent_message_id = thread.parent_message_id;
    body.ref_file_ids = ref_file_ids.to_vec();
    body.model_type = upstream_model_type(model);
    body
}

/// Map a client-facing model id onto the value DeepSeek expects.
///
/// The public ids are upper case (`EXPERT`) because that is what the OpenAI
/// model list advertises; upstream wants the lower-case enum from
/// aiodeepseek's `ModelType` (`default` / `expert` / `vision`). Before this the
/// body always said `default`, so picking a model changed nothing for text and
/// only affected the `x-model-type` header on image upload.
fn upstream_model_type(model: &str) -> String {
    let lowered = model.trim().to_ascii_lowercase();
    match lowered.as_str() {
        // `deepseek-chat` and friends are the generic chat ids: they are the
        // default model, not an unknown one.
        "deepseek-chat" | "deepseek-v3" | "deepseek-reasoner" | "" => "default".to_string(),
        other => other.to_string(),
    }
}

/// Replace the remote session after a context-limit rejection.
///
/// DeepSeek keeps the conversation server-side, so an overflowing session
/// cannot be trimmed from here: create a fresh one and let the caller replay
/// the whole transcript into it. Persisted resume state is reset to match.
async fn rebuild_session(
    state: &AppState,
    key: &str,
    reason: session::RebuildReason,
) -> Result<ThreadState, Response> {
    session::open_session(state, key, session::ThreadChange::Rebuild { reason })
        .await
        .map_err(ProxyError::into_response)
}

fn content_text(content: &Option<Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .map(|p| match p.get("type").and_then(|t| t.as_str()) {
                Some("image_url") => "[image]".to_string(),
                Some("text") => p
                    .get("text")
                    .and_then(|t| t.as_str())
                    .unwrap_or_default()
                    .to_string(),
                // Python falls back to `str(part)` for unknown part types.
                _ => p.to_string(),
            })
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
/// The part of an email before the `@`, for log lines.
fn short_account(email: &str) -> &str {
    email.split('@').next().unwrap_or(email)
}

fn thread_key(headers: &HeaderMap, req: &ChatRequest) -> String {
    if let Some(v) = req
        .thread_id
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        return v.to_string();
    }
    if let Some(v) = header_value(headers, "x-thread-id") {
        return v;
    }
    // Title calls must be detected before the session id is consulted.
    let prefix = if is_title_request(req) {
        "title_"
    } else {
        "thread_"
    };
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

/// Label for one message: `user`, `assistant`, `system`, or `tool <name>`.
/// Render one message the way `prompts.py::build_prompt` does.
///
/// The role prefixes are byte-identical to Python's `[prompts]` table: the
/// model was anchored on this framing, so `[user]:` instead of `User: ` is a
/// real behavioural difference, not cosmetics.
fn render_message(m: &Msg, wire: dialect::Dialect) -> Option<String> {
    let body = content_text(&m.content);
    match m.role.as_str() {
        "system" | "developer" => {
            if body.is_empty() {
                return None;
            }
            Some(format!("System: {body}"))
        }
        "user" => {
            if body.is_empty() {
                return None;
            }
            Some(format!("User: {body}"))
        }
        "assistant" => {
            if m.tool_calls.is_empty() {
                if body.is_empty() {
                    return None;
                }
                return Some(format!("Assistant: {body}"));
            }
            let calls = dialect::format_tool_calls_for_history_with(&m.tool_calls, wire);
            if calls.is_empty() {
                return if body.is_empty() {
                    None
                } else {
                    Some(format!("Assistant: {body}"))
                };
            }
            // Python emits the prose and the calls as two parts.
            Some(if body.is_empty() {
                format!("Assistant:\n{calls}")
            } else {
                format!("Assistant: {body}\n\nAssistant:\n{calls}")
            })
        }
        "tool" => {
            let id = m.tool_call_id.as_deref().unwrap_or("");
            Some(wire.render_tool_result(id, &body))
        }
        _ => {
            if body.is_empty() {
                None
            } else {
                Some(format!("{}: {body}", m.role))
            }
        }
    }
}

/// Python's `CONTINUE_AFTER_TOOL`, appended when a turn ends on a tool result.
const CONTINUE_AFTER_TOOL: &str = "Now continue with the task based on the tool result above.";

/// Told to the model when the backend lost the reply we were parented to.
///
/// The turn is resent on top of the previous cursor, so the model already has
/// everything up to that point. Without this line it sees a prompt that appears
/// to repeat the conversation and answers as if the last exchange never
/// happened; with it, it picks up where the lost reply would have been.
const LOST_REPLY_NOTE: &str = "[proxy] The previous assistant reply was lost on the server side. \
     Continue the conversation from the message above; do not repeat it.";

/// Fresh thread: flatten the whole conversation, then the continuation line
/// when the last message is a tool result (Python's `last_role == "tool"`).
fn flatten_transcript(messages: &[Msg], wire: dialect::Dialect) -> String {
    let blocks: Vec<String> = messages
        .iter()
        .filter_map(|m| render_message(m, wire))
        .collect();
    let mut out = blocks.join("\n\n");
    if last_non_system_role(messages) == Some("tool") {
        out.push_str("\n\n");
        out.push_str(CONTINUE_AFTER_TOOL);
    }
    out
}

/// Messages since the last assistant reply, i.e. the turn's new input.
///
/// Port of `prompts.get_new_messages()`: everything after the last assistant
/// message. Both the prompt and the image intake read from this one slice, so
/// an attached image can never belong to a different turn than the text.
fn delta_messages(messages: &[Msg]) -> &[Msg] {
    let start = messages
        .iter()
        .rposition(|m| m.role == "assistant")
        .map(|i| i + 1)
        .unwrap_or(0);
    &messages[start..]
}

fn last_non_system_role(messages: &[Msg]) -> Option<&str> {
    messages
        .iter()
        .rev()
        .find(|m| m.role != "system" && m.role != "developer")
        .map(|m| m.role.as_str())
}

/// Build the continuation prompt for a thread that already exists.
///
/// Port of Python's `build_prompt(new_messages, ...)` where `new_messages` is
/// `get_new_messages()`: every delta message is rendered with its role prefix,
/// in order, and `CONTINUE_AFTER_TOOL` is appended when the delta ends on a
/// tool result. Rendering the whole delta (rather than just the newest user
/// message) is what makes the tool round trip work: the `role: "tool"`
/// messages carry the results the model's call returned.
fn continuation_prompt(delta: &[Msg], wire: dialect::Dialect) -> Option<String> {
    let blocks: Vec<String> = delta
        .iter()
        .filter_map(|m| render_message(m, wire))
        .collect();
    if blocks.is_empty() {
        return None;
    }
    let mut out = blocks.join("\n\n");
    if last_non_system_role(delta) == Some("tool") {
        out.push_str("\n\n");
        out.push_str(CONTINUE_AFTER_TOOL);
    }
    Some(out)
}

async fn healthz() -> Json<Value> {
    Json(json!({"status": "ok"}))
}

/// Python's `/readyz`: login is lazy, so being up is being ready.
async fn readyz(State(state): State<AppState>) -> Response {
    // Flip to 503 the moment shutdown starts, so a load balancer or the CLI's
    // `start` health probe stops sending new turns while the old ones drain.
    if state.stats.shutting_down.load(Ordering::Relaxed) {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"status": "draining", "backend": "lazy_login"})),
        )
            .into_response();
    }
    Json(json!({"status": "ready", "backend": "lazy_login"})).into_response()
}

/// Unknown route: `404` in the OpenAI error envelope, so SDK clients parse it
/// instead of choking on axum's empty-body default.
async fn fallback() -> Response {
    ProxyError::new(
        "Not Found",
        axum::http::StatusCode::NOT_FOUND,
        "not_found",
    )
    .into_response()
}

/// Echo `x-request-id`, or mint one, and carry it in every log line.
///
/// The id lives in a tracing span rather than in a handler parameter, so the
/// turn's own events inherit `req=...` without every call site learning a new
/// argument. An inbound id is honoured only when it is short and printable: it
/// ends up in logs, so arbitrary client bytes must not get there.
/// Browser clients: answer the preflight and let any origin call the API.
///
/// The proxy is authenticated by its own bearer keys, not by the browser's
/// origin, so an allowlist here would add a failure mode without adding a
/// guarantee. A local UI (Open WebUI, a static page) otherwise cannot reach
/// the API at all: its preflight gets a 405.
async fn cors(req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let mut resp = if req.method() == axum::http::Method::OPTIONS {
        axum::http::StatusCode::NO_CONTENT.into_response()
    } else {
        next.run(req).await
    };
    let headers = resp.headers_mut();
    let mut allow = |name: &'static str, value: &'static str| {
        if let Ok(v) = axum::http::HeaderValue::from_str(value) {
            headers.insert(name, v);
        }
    };
    // `*` is fine because credentials are a header, never a cookie.
    allow("access-control-allow-origin", "*");
    allow(
        "access-control-allow-headers",
        "authorization, content-type, x-request-id, x-session-id, x-thread-id, x-session-affinity",
    );
    allow("access-control-allow-methods", "GET, POST, OPTIONS");
    // Lets a browser client read the id it must quote in a bug report.
    allow("access-control-expose-headers", "x-request-id, retry-after");
    allow("access-control-max-age", "86400");
    resp
}

/// `501` in the OpenAI envelope for endpoints this proxy does not implement.
async fn not_supported(req: axum::extract::Request) -> Response {
    ProxyError::new(
        format!(
            "{} is not supported by deepseek-rs; see the README for what is",
            req.uri().path()
        ),
        axum::http::StatusCode::NOT_IMPLEMENTED,
        "not_supported",
    )
    .into_response()
}

async fn request_id(req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    const HEADER: &str = "x-request-id";
    let inbound = req
        .headers()
        .get(HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty() && v.len() <= 128 && v.chars().all(|c| c.is_ascii_graphic()))
        .map(str::to_string);
    let id = inbound.unwrap_or_else(uuid_like);

    let span = tracing::info_span!("turn", req = %id);
    let mut resp = next.run(req).instrument(span).await;
    if let Ok(v) = axum::http::HeaderValue::from_str(&id) {
        resp.headers_mut().insert(HEADER, v);
    }
    resp
}

/// Known path, wrong method: same envelope, `405`.
async fn method_not_allowed() -> Response {
    ProxyError::new(
        "Method Not Allowed",
        axum::http::StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
    )
    .into_response()
}

/// `GET /admin/throttled` — threads whose bound account rate-limited them.
///
/// Most recent first. The account's *live* cooldown lives in the pool, not the
/// database, so it is joined here: a thread is worth rebinding when its account
/// is still cooling down, not merely because it failed once an hour ago.
async fn admin_throttled(State(state): State<AppState>) -> Response {
    let rows = match state.store.throttled_threads() {
        Ok(rows) => rows,
        Err(e) => return backend_error(&format!("store: {e}")),
    };
    let accounts = state.pool().snapshot().await;
    let live: HashMap<&str, (Option<u64>, bool)> = accounts
        .iter()
        .map(|a| (a.email.as_str(), (a.cooldown_ms, a.healthy)))
        .collect();
    let now = unix_secs();

    let threads: Vec<Value> = rows
        .iter()
        .map(|r| {
            let (cooldown_ms, healthy) = live
                .get(r.email.as_str())
                .copied()
                .unwrap_or((None, false));
            json!({
                "thread": r.thread_id,
                "account": r.email,
                "throttled_at": r.throttled_at,
                "throttled_secs_ago": now.saturating_sub(r.throttled_at.max(0) as u64),
                "account_cooldown_ms": cooldown_ms,
                "account_healthy": healthy,
            })
        })
        .collect();

    Json(json!({ "count": threads.len(), "threads": threads })).into_response()
}

/// `POST /admin/rebind` — move one thread, or every throttled thread, to a
/// fresh remote session, optionally on another account.
///
/// `account` accepts `next` (least-crowded healthy other account), `fresh`
/// (same account, new session), or an email. `dry_run` returns the plan without
/// touching anything, because a bulk rebind replays every transcript it moves.
#[derive(Deserialize)]
struct RebindRequest {
    #[serde(default)]
    thread: Option<String>,
    #[serde(default)]
    account: Option<String>,
    #[serde(default)]
    all_throttled: bool,
    #[serde(default)]
    dry_run: bool,
    #[serde(default)]
    limit: Option<usize>,
}

/// Upper bound on a bulk rebind. Every thread moved costs one replay.
const REBIND_BULK_LIMIT: usize = 10;

fn parse_target(account: Option<&str>) -> Option<(session::ThreadChange, String)> {
    let raw = account.unwrap_or("next").trim();
    match raw {
        "" | "next" => Some((
            session::ThreadChange::Rebind {
                target: pool::AccountTarget::Next,
                reason: session::RebuildReason::Manual,
            },
            "next".to_string(),
        )),
        "fresh" | "current" => Some((
            session::ThreadChange::Rebuild {
                reason: session::RebuildReason::Manual,
            },
            "fresh".to_string(),
        )),
        email if email.contains('@') => Some((
            session::ThreadChange::Rebind {
                target: pool::AccountTarget::Email(email.to_string()),
                reason: session::RebuildReason::Manual,
            },
            email.to_string(),
        )),
        _ => None,
    }
}

async fn admin_rebind(
    State(state): State<AppState>,
    Json(body): Json<RebindRequest>,
) -> Response {
    let Some((change, target_label)) = parse_target(body.account.as_deref()) else {
        return ProxyError::new(
            "account must be next, fresh, or an email",
            axum::http::StatusCode::BAD_REQUEST,
            "invalid_request",
        )
        .with_param("account")
        .into_response();
    };

    // Which threads? An explicit one wins over the bulk flag.
    let targets: Vec<String> = match (&body.thread, body.all_throttled) {
        (Some(t), _) => vec![t.clone()],
        (None, true) => {
            let rows = match state.store.throttled_threads() {
                Ok(rows) => rows,
                Err(e) => return backend_error(&format!("store: {e}")),
            };
            let cap = body.limit.unwrap_or(REBIND_BULK_LIMIT).min(REBIND_BULK_LIMIT);
            rows.into_iter().take(cap).map(|r| r.thread_id).collect()
        }
        (None, false) => {
            return ProxyError::new(
                "thread is required unless all_throttled is true",
                axum::http::StatusCode::BAD_REQUEST,
                "invalid_request",
            )
            .with_param("thread")
            .into_response()
        }
    };

    if body.dry_run {
        let plan: Vec<Value> = targets
            .iter()
            .map(|t| {
                json!({
                    "thread": t,
                    "from": state.store.get_binding(t).ok().flatten(),
                    "to": target_label,
                })
            })
            .collect();
        return Json(json!({ "dry_run": true, "count": plan.len(), "plan": plan }))
            .into_response();
    }

    let mut done: Vec<Value> = Vec::new();
    let mut failed: Vec<Value> = Vec::new();
    for thread in targets {
        let from = state.store.get_binding(&thread).ok().flatten();
        // Serialize against an in-flight turn on this thread: replacing the
        // session under a streaming turn would orphan its cursor.
        let _turn = state.threads.lock(&thread).await;
        match session::open_session(&state, &thread, clone_change(&change)).await {
            Ok(fresh) => {
                // A rebind makes the old throttle flag meaningless; `bind`
                // clears it when the account changes, but a same-account
                // `fresh` rebuild must clear it explicitly.
                if let Err(e) = state.store.clear_throttled(&thread) {
                    tracing::warn!(thread = %thread, error = %e, "could not clear throttle flag");
                }
                done.push(json!({
                    "thread": thread,
                    "from": from,
                    "to": state.store.get_binding(&thread).ok().flatten(),
                    "session": fresh.session_id,
                }));
            }
            Err(e) => failed.push(json!({ "thread": thread, "error": e.message })),
        }
    }

    let status = if failed.is_empty() {
        axum::http::StatusCode::OK
    } else if done.is_empty() {
        axum::http::StatusCode::BAD_GATEWAY
    } else {
        axum::http::StatusCode::MULTI_STATUS
    };
    (status, Json(json!({ "rebound": done, "failed": failed }))).into_response()
}

/// `ThreadChange` is consumed by `open_session`; the bulk loop needs one per
/// thread, so rebuild it from the account label.
fn clone_change(change: &session::ThreadChange) -> session::ThreadChange {
    match change {
        session::ThreadChange::Fresh => session::ThreadChange::Fresh,
        session::ThreadChange::Rebuild { reason } => {
            session::ThreadChange::Rebuild { reason: *reason }
        }
        session::ThreadChange::Rebind { target, reason } => session::ThreadChange::Rebind {
            target: target.clone(),
            reason: *reason,
        },
    }
}

/// `x-deepseek-rs-rebind`: `next`, `fresh`, or an account email.
///
/// This is the deterministic channel a pi extension uses: the extension eats
/// the operator's sentinel in the `input` hook, so the sentinel never becomes a
/// message, and sets this header on the request that follows. The proxy then
/// never scans message text for directives, and nothing has to be stripped
/// from the transcript.
fn rebind_header(headers: &HeaderMap) -> Result<Option<session::ThreadChange>, ProxyError> {
    let Some(raw) = header_value(headers, "x-deepseek-rs-rebind") else {
        return Ok(None);
    };
    // An unknown target is a client error: a silent no-op would leave the
    // caller believing the session moved.
    let (change, _) = parse_target(Some(&raw)).ok_or_else(|| {
        ProxyError::new(
            "x-deepseek-rs-rebind must be next, fresh, or an account email",
            axum::http::StatusCode::BAD_REQUEST,
            "invalid_request",
        )
        .with_param("x-deepseek-rs-rebind")
    })?;
    Ok(Some(change))
}

/// When the running executable was built, as `YYYY-MM-DD HH:MM:SS`.
///
/// There is no build script, and nothing else at startup says which binary is
/// live — which cost an hour of log archaeology once: a stale `release` build
/// from two days earlier was running, so a newly added control header was
/// silently ignored and every feature landed since was simply absent. The
/// mtime is the cheapest honest answer to "is the process I am looking at the
/// code I just wrote?".
fn build_id() -> String {
    let Ok(exe) = std::env::current_exe() else {
        return "unknown".to_string();
    };
    let Ok(meta) = std::fs::metadata(&exe) else {
        return "unknown".to_string();
    };
    let Ok(modified) = meta.modified() else {
        return "unknown".to_string();
    };
    let Ok(since) = modified.duration_since(std::time::UNIX_EPOCH) else {
        return "unknown".to_string();
    };
    let secs = since.as_secs() as i64;
    // `chrono` is not a dependency here; format by hand from the epoch offset.
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Days since the Unix epoch to a proleptic Gregorian date (Howard Hinnant's
/// `civil_from_days`), so the log line needs no date library.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `GET /stats` — everything `deepseek-rs stats` and `status` need.
async fn stats(State(state): State<AppState>) -> Json<Value> {
    let s = &state.stats;
    let counts = state.store.counts().unwrap_or_default();
    let threads_in_memory = state.threads.cached_len();

    let accounts: Vec<Value> = state
        .pool()
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
                "breaker": a.breaker.as_str(),
                "retry_in_secs": a.retry_in_secs,
            })
        })
        .collect();

    let (config_generation, config_age_secs, config_last_error) = state.settings.snapshot();

    Json(json!({
        "pid": std::process::id(),
        "uptime_secs": s.uptime_secs(),
        "model": state.settings.get().model_name,
        "thinking": state.settings.get().thinking_default,
        "search": state.settings.get().search_enabled,
        // Reload state, so `deepseek-rs stats` answers "did my SIGHUP take?"
        // and "is the running config the last good one?" without reading logs.
        "config": {
            "generation": config_generation,
            "loaded_at_secs_ago": config_age_secs,
            "last_error": config_last_error,
        },
        "requests": {
            "total": s.total.load(Ordering::Relaxed),
            "ok": s.ok.load(Ordering::Relaxed),
            "errors": s.errors.load(Ordering::Relaxed),
            "auth_failed": s.auth_failed.load(Ordering::Relaxed),
            "retries": s.retries.load(Ordering::Relaxed),
            "tool_calls": s.tool_calls.load(Ordering::Relaxed),
            "images_dropped": s.images_dropped.load(Ordering::Relaxed),
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

/// `GET /v1/models` — the same three ids Python advertises, in the same shape.
async fn models(_state: State<AppState>) -> Json<Value> {
    Json(json!({
        "object": "list",
        "data": [
            {"id": "DEFAULT", "object": "model", "created": 0, "owned_by": "deepseek"},
            {"id": "EXPERT", "object": "model", "created": 0, "owned_by": "deepseek"},
            {"id": "VISION", "object": "model", "created": 0, "owned_by": "deepseek"},
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
        // `require_api_key` raises FastAPI's HTTPException, whose body is
        // `{"detail": ...}` — not the OpenAI error envelope.
        Ok(false) => Err(unauthorized(presented.is_empty())),
        Err(e) => Err(backend_error(&format!("store: {e}"))),
    }
}

/// `401` in the OpenAI envelope, not FastAPI's `{"detail": ...}`.
///
/// Official SDKs parse the `error` object to decide whether a failure is
/// retryable; `detail` makes every auth failure look like an unparseable body.
/// This is the one intentional break from the Python port (PLAN.md T-01).
fn unauthorized(missing: bool) -> Response {
    let (message, code) = if missing {
        ("Missing bearer token", "missing_api_key")
    } else {
        ("Invalid API key", "invalid_api_key")
    };
    let mut resp = ProxyError::new(message, axum::http::StatusCode::UNAUTHORIZED, code)
        .into_response();
    if let Ok(v) = axum::http::HeaderValue::from_str("Bearer") {
        resp.headers_mut()
            .insert(axum::http::header::WWW_AUTHENTICATE, v);
    }
    resp
}

/// Why a completion attempt ultimately failed.
#[derive(Debug, Clone)]
enum CompletionError {
    /// Still throttled after every retry; maps to 429 `rate_limit_exceeded`.
    RateLimited {
        retry_after_s: u64,
    },
    /// Login is dead; maps to 502 `backend_auth_failed`.
    LoginDead(String),
    /// Queue full or queue wait timed out; maps to 503 `queue_unavailable`.
    QueueUnavailable(String),
    Other(String),
}

impl From<pool::PoolError> for CompletionError {
    fn from(e: pool::PoolError) -> Self {
        match e {
            pool::PoolError::Queue(m) => CompletionError::QueueUnavailable(m),
            pool::PoolError::LoginDead(m) => CompletionError::LoginDead(m),
            pool::PoolError::RateLimited { retry_after_s } => {
                CompletionError::RateLimited { retry_after_s }
            }
            pool::PoolError::Other(m) => CompletionError::Other(m),
        }
    }
}

impl std::fmt::Display for CompletionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CompletionError::RateLimited { retry_after_s } => write!(
                f,
                "DeepSeek backend rate-limited this turn (retry after {retry_after_s}s)"
            ),
            CompletionError::LoginDead(msg) | CompletionError::Other(msg) => f.write_str(msg),
            CompletionError::QueueUnavailable(msg) => f.write_str(msg),
        }
    }
}

/// Public API error. Port of `ProxyError` in `deepseek_proxy/errors.py`.
///
/// Backend detail stays in the logs; the client gets a stable code and a
/// generic message, exactly as Python does.
#[derive(Debug, Clone)]
struct ProxyError {
    message: String,
    status: axum::http::StatusCode,
    code: &'static str,
    param: Option<String>,
    retry_after: Option<u64>,
}

impl ProxyError {
    fn new(message: impl Into<String>, status: axum::http::StatusCode, code: &'static str) -> Self {
        Self {
            message: message.into(),
            status,
            code,
            param: None,
            retry_after: None,
        }
    }

    fn with_param(mut self, param: impl Into<String>) -> Self {
        self.param = Some(param.into());
        self
    }

    /// `ProxyError.payload()`: `type` follows the status, not the code.
    fn payload(&self) -> Value {
        json!({
            "error": {
                "message": self.message,
                "type": match self.status.as_u16() {
                    400..=404 => "invalid_request_error",
                    429 => "rate_limit_error",
                    _ => "server_error",
                },
                "param": self.param,
                "code": self.code,
            }
        })
    }

    fn into_response(self) -> Response {
        let retry_after = self.retry_after;
        let mut resp = (self.status, Json(self.payload())).into_response();
        // `app.py` sets Retry-After whenever the exception carries one.
        if let Some(retry_after) = retry_after {
            if let Ok(v) = axum::http::HeaderValue::from_str(&retry_after.max(1).to_string()) {
                resp.headers_mut().insert("retry-after", v);
            }
        }
        resp
    }
}

/// Port of `error_for()` in `deepseek_proxy/transport.py`: the single place
/// that turns a backend failure into what the client sees and how it is logged.
async fn error_for(state: &AppState, key: &str, err: CompletionError) -> ProxyError {
    match err {
        CompletionError::RateLimited { retry_after_s } => {
            let mut e = ProxyError::new(
                "Backend capacity temporarily unavailable",
                axum::http::StatusCode::TOO_MANY_REQUESTS,
                "rate_limit_exceeded",
            );
            e.retry_after = Some(retry_after_s.max(1));
            e
        }
        CompletionError::LoginDead(msg) => {
            tracing::error!(thread = %key, error = %msg, "backend login failed");
            if state.pool().failover(key).await.is_some() {
                // The account binding just moved, so the session id - scoped to
                // the account that created it - now points at nothing. Forget
                // it and let the next turn open a session on the new account,
                // instead of burning a turn to discover the mismatch.
                state.threads.forget(key);
            }
            ProxyError::new(
                "Backend login failed; disabled account, retry with a fresh thread",
                axum::http::StatusCode::BAD_GATEWAY,
                "backend_auth_failed",
            )
        }
        CompletionError::QueueUnavailable(msg) => {
            tracing::warn!(thread = %key, error = %msg, "account queue unavailable");
            ProxyError::new(
                "Request queue unavailable; try again later",
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                "queue_unavailable",
            )
        }
        CompletionError::Other(msg) => {
            tracing::error!(thread = %key, error = %msg, "backend request failed");
            ProxyError::new(
                "Backend request failed",
                axum::http::StatusCode::BAD_GATEWAY,
                "backend_error",
            )
        }
    }
}

/// Send the completion, retrying throttles with linear backoff.
///
/// Port of `Backend.generate()` in `deepseek_proxy/backend.py`. Three details
/// are deliberately identical:
///
/// - only throttle errors retry; anything else is returned immediately;
/// - the wait is `backoff * min(attempt + 1, max(1, max_retries))`, not an
///   exponential doubling;
/// - the wait is applied by cooling the account down, so `acquire()` sleeps.
///   A second timer would race the gate and fire the retry early.
///
/// Only pre-first-byte failures are retried, which is safe for both stream and
/// non-stream callers; a mid-stream failure is surfaced as-is.
async fn completion_with_retry(
    state: &AppState,
    thread_id: &str,
    body: &CompletionBody,
) -> Result<(reqwest::Response, pool::RateTicket), CompletionError> {
    state
        .pool()
        .complete(
            thread_id,
            body,
            state.settings.get().rate_limit_max_retries,
            state.settings.get().rate_limit_backoff_secs,
            |attempt, wait| {
                state.stats.retries.fetch_add(1, Ordering::Relaxed);
                tracing::info!(
                    thread = %thread_id,
                    attempt,
                    retry_delay_s = wait,
                    "retrying after transient backend error"
                );
            },
        )
        .await
        .map_err(|e| {
            if matches!(e, pool::PoolError::RateLimited { .. }) {
                state.stats.errors.fetch_add(1, Ordering::Relaxed);
            }
            CompletionError::from(e)
        })
}

/// Run correction turns for malformed tool calls.
///
/// Port of the repair loop in `deepseek_proxy/service.py`, including the two
/// subtleties that took a bug to find there:
///
/// - a repaired call that repeats an already-accepted call is dropped, because
///   the model tends to echo the block it was just told about;
/// - an attempt that resolves nothing records ONE `repair_omitted_call` marker
///   instead of re-listing the same failures. Re-listing grows `expected` every
///   attempt until the retry budget is gone.
///
/// Repair turns are non-streaming and never carry thinking/search, matching
/// `turn.generate(repair_prompt, stream=False)`.
/// Port of `deepseek_proxy/tools/recovery.py::Recovery`.
///
/// The validation and recovery policy is not "does the block parse?" — every
/// call is checked against the offered tool's JSON Schema before it is
/// accepted, on the first turn as well as during repair. A call that names an
/// unknown tool, whose arguments are not a JSON object, or whose arguments do
/// not match the schema is converted into a repair failure instead of being
/// forwarded to the client.
struct Recovery {
    specs: Vec<dialect::ToolSpec>,
    /// Wire dialect; repair restates the matching block shape.
    dialect: dialect::Dialect,
    calls: Vec<dialect::ToolCall>,
    pending: Vec<dialect::MalformedCall>,
    required: bool,
    parallel: bool,
    previous: HashMap<(String, String), usize>,
    repair_seen: HashMap<(String, String), usize>,
    expected: usize,
    resolved: usize,
    repairing: bool,
    /// The failure list of the attempt currently being repaired, kept so a
    /// retry can restate what actually went wrong.
    last_failures: Vec<dialect::MalformedCall>,
}

impl Recovery {
    fn new(
        specs: Vec<dialect::ToolSpec>,
        required: bool,
        parallel: bool,
        dialect: dialect::Dialect,
    ) -> Self {
        Self {
            specs,
            dialect,
            calls: Vec::new(),
            pending: Vec::new(),
            required,
            parallel,
            previous: HashMap::new(),
            repair_seen: HashMap::new(),
            expected: 0,
            resolved: 0,
            repairing: false,
            last_failures: Vec::new(),
        }
    }

    /// Feed one event; returns the calls it produced (zero or one).
    ///
    /// Mirrors `Recovery.accept()`: malformed events join `pending`, text is
    /// ignored, and a parsed call is schema-validated before acceptance.
    fn accept(
        &mut self,
        event: dialect::ScanEvent,
    ) -> Result<Option<dialect::ToolCall>, ProxyError> {
        let call = match event {
            dialect::ScanEvent::Malformed(m) => {
                self.pending.push(m);
                return Ok(None);
            }
            dialect::ScanEvent::Text(_) => return Ok(None),
            dialect::ScanEvent::Tool(c) => c,
        };

        if let Some(reason) = dialect::validation_reason(&self.specs, &call) {
            self.pending.push(dialect::MalformedCall::new(
                Some(call.name.clone()),
                call.arguments.clone(),
                reason,
            ));
            return Ok(None);
        }

        let key = dialect::fingerprint(&call);
        if self.repairing {
            let seen = self.repair_seen.entry(key.clone()).or_insert(0);
            *seen += 1;
            let before = self.previous.get(&key).copied().unwrap_or(0);
            if *seen <= before {
                // The repair echoed a previously accepted call.
                return Ok(None);
            }
        }
        if !self.parallel && !self.calls.is_empty() {
            return Err(ProxyError::new(
                "Backend produced multiple calls with parallel_tool_calls=false",
                axum::http::StatusCode::BAD_GATEWAY,
                "tool_repair_failed",
            ));
        }
        self.calls.push(call.clone());
        self.resolved += 1;
        Ok(Some(call))
    }

    /// `tool_choice: "required"` must yield at least one call.
    fn require_call(&mut self) {
        if self.required && self.calls.is_empty() && self.pending.is_empty() {
            self.pending.push(dialect::MalformedCall::new(
                None,
                String::new(),
                "required_tool_call_missing",
            ));
        }
    }

    /// Port of `begin_repair()`: build the prompt and reset the per-attempt
    /// counters. `previous` snapshots every call accepted so far, so the next
    /// repair cannot re-accept an echo of one of them.
    fn begin_repair(&mut self, attempt: usize) -> String {
        let mut prompt =
            dialect::build_repair_prompt_for(&self.dialect, &self.pending, &self.specs, attempt);
        // Schemas only on the first attempt. They are the expensive part of a
        // repair prompt, and they answer "what shape were the arguments?" -
        // a question the model has already been given the answer to by the time
        // it fails a second time. Repeating them adds bulk to exactly the turn
        // where the context is already long, and the ladder is trying to get
        // *shorter* as attempts are spent.
        if attempt <= 1 {
            // Only the schema of a tool the model actually named and got wrong
            // on its arguments. Sending the whole catalogue here makes a repair
            // turn larger than the turn it repairs.
            let schemas = dialect::repair_schemas(&self.pending, &self.specs);
            if !schemas.is_empty() {
                prompt.push_str("\nSchema(s) for the tool call(s) that failed:\n");
                prompt.push_str(&dialect::py_json_dumps(&Value::Array(schemas)));
            }
        }
        self.expected = self.pending.len();
        // Keep the detail: a failed retry must be able to say which tool and
        // which reason, not just "something was omitted".
        self.last_failures = self.pending.clone();
        self.pending.clear();
        self.resolved = 0;
        self.previous.clear();
        for call in &self.calls {
            *self.previous.entry(dialect::fingerprint(call)).or_insert(0) += 1;
        }
        self.repair_seen.clear();
        self.repairing = true;
        prompt
    }

    /// Port of `end_repair()`: an attempt that resolved nothing keeps the
    /// failure list it was given.
    ///
    /// The Python original collapsed the list into a single nameless
    /// `repair_omitted_call`, which made every retry weaker than the attempt
    /// before it: the model was no longer told which tool it had got wrong or
    /// why. Restating the same list is bounded (`expected` is set from it, not
    /// accumulated), so the growth the original guarded against cannot happen.
    fn end_repair(&mut self) {
        if self.resolved < self.expected {
            let unresolved = self.expected - self.resolved;
            let mut carried: Vec<dialect::MalformedCall> = self
                .last_failures
                .iter()
                .take(unresolved)
                .cloned()
                .collect();
            if carried.is_empty() {
                // Nothing was described to begin with (a required call that
                // never arrived); a marker is all that can be said.
                carried.push(dialect::MalformedCall::new(
                    None,
                    String::new(),
                    "repair_omitted_call",
                ));
            }
            self.pending.extend(carried);
        }
    }

    fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }
}

/// How `tool_choice` narrows the offered tools and sets the `required` flag.
///
/// Port of the filtering at the top of `ChatService.events()`:
/// `"none"` drops every tool, a named choice keeps only that function and
/// implies required, and `"required"` keeps all tools and implies required.
fn resolve_tools(req: &ChatRequest) -> (Vec<dialect::ToolSpec>, bool) {
    let mut tools = req.tools.clone();
    let mut required = false;
    match req.tool_choice.as_ref() {
        Some(Value::String(s)) if s == "none" => tools.clear(),
        Some(Value::String(s)) if s == "required" => required = true,
        Some(Value::Object(obj)) => {
            if let Some(name) = obj
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
            {
                tools.retain(|t| {
                    t.get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(|n| n.as_str())
                        == Some(name)
                });
                required = true;
            }
        }
        _ => {}
    }
    (dialect::tool_specs(&tools), required)
}

/// Wait before retry number `attempt` (0-based).
///
/// Python: `backoff * min(attempt + 1, max(1, max_retries))`, with the base
/// floored at 0.5s by `Backend.generate()`.
fn backoff_delay(settings: &Settings, attempt: u32) -> f64 {
    let max = settings.rate_limit_max_retries.max(1);
    settings.rate_limit_backoff_secs.max(0.5) * f64::from((attempt + 1).min(max))
}

/// HTTP response for a failed completion attempt, via `error_for()`.
async fn completion_error_response(state: &AppState, key: &str, err: CompletionError) -> Response {
    error_for(state, key, err).await.into_response()
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
    // `ChatService.validate()` runs before anything is queued: a malformed
    // request must not take the thread lock or create a remote session.
    if let Err(e) = validate_request(&req) {
        state.stats.errors.fetch_add(1, Ordering::Relaxed);
        return e.into_response();
    }
    let key = thread_key(&headers, &req);
    // Serialize turns on this thread. Taken BEFORE the parent id is read and
    // held until the turn (plus its correction turns) is finished, so a second
    // request on the same thread waits instead of forking the conversation.
    let turn = state.threads.lock(&key).await;
    let model = req
        .model
        .clone()
        .unwrap_or_else(|| state.settings.get().model_name.clone());

    // `ChatService.validate()`: hermes sends the public OpenAI id, the backend
    // uses internal names. Keep explicit expert/vision selection and fold the
    // common chat ids onto the default model; reject anything else.
    if !turn::accepts_model(&model) {
        state.stats.errors.fetch_add(1, Ordering::Relaxed);
        return ProxyError::new(
            "Unknown model; see /v1/models",
            axum::http::StatusCode::BAD_REQUEST,
            "model_not_found",
        )
        .with_param("model")
        .into_response();
    }
    let bound = state.store.get_binding(&key).ok().flatten();
    tracing::info!(
        thread = %key,
        account = %bound.as_deref().map_or("new", short_account),
        model = %model,
        stream = req.stream,
        messages = req.messages.len(),
        tools = req.tools.len(),
        "request in"
    );
    state.stats.total.fetch_add(1, Ordering::Relaxed);

    // A rebind directive must be resolved before preparation: it decides
    // whether this turn is a continuation (delta) or a replay into a session
    // that has never seen the conversation.
    let rebind = match rebind_header(&headers) {
        Ok(r) => r,
        Err(e) => {
            state.stats.errors.fetch_add(1, Ordering::Relaxed);
            return e.into_response();
        }
    };

    // Everything up to the upstream call is preparation, and it lives in one
    // module: resume, prompts, session create, image upload, body.
    let prepared = match turn::prepare(&state, &key, &req, &model, rebind).await {
        Ok(p) => p,
        Err(e) => {
            state.stats.errors.fetch_add(1, Ordering::Relaxed);
            return e.into_response();
        }
    };
    let turn::Prepared {
        thread,
        body,
        replay,
        can_recover,
        thinking,
        specs,
        required,
        dialect: wire,
    } = prepared;

    let id = format!("chatcmpl-{}", uuid_like());
    let created = unix_secs();

    let (resp, slot) = match completion_with_retry(&state, &key, &body).await {
        Ok(r) => r,
        Err(e) => return completion_error_response(&state, &key, e).await,
    };

    tracing::debug!(thread = %key, thinking, parent = ?thread.parent_message_id, "upstream completion accepted");

    stream_response(LiveTurn {
        guard: turn,
        state,
        key,
        resp,
        slot,
        body,
        id,
        model,
        created,
        replay,
        can_recover,
        // A non-streaming client still gets exact token counts: ask the pump
        // for the trailing usage chunk and let the collector fold it in.
        include_usage: !req.stream || turn::include_usage(req.stream_options.as_ref()),
        started,
        specs,
        required,
        parallel: req.parallel_tool_calls,
        stream: req.stream,
        dialect: wire,
    })
    .await
}

/// Send an OpenAI-shaped SSE error event and stop the stream.
///
/// The old behaviour was to log a fatal hint and then emit `finish_reason:
/// stop`, so clients saw an empty but *successful* completion. That is why the
/// errors never showed up anywhere.
/// Send a `ProxyError` payload as a final SSE event, then `[DONE]`.
///
/// Python's `stream()` does exactly this: `yield sse(event.payload())` followed
/// by `data: [DONE]`, so a mid-stream failure is still an error the client can
/// see rather than a clean `finish_reason: stop`.
async fn send_stream_error(tx: &sse::ChunkSender, err: ProxyError) {
    // The status only means something before the first byte; the SSE transport
    // sends the envelope, the JSON collector turns it into a status code.
    let _ = tx.send(sse::Chunk::Error(err.status, err.payload())).await;
    let _ = tx.send(sse::Chunk::Data("[DONE]".into())).await;
}

/// Fail a turn without losing the cursor.
///
/// Every upstream turn leaves messages in the session, even when it errors. If
/// the cursor were not saved here, the next request would resume from the
/// previous message and DeepSeek would render it as an edit branching off
/// mid-thread — the symptom this replaces.
async fn fail_turn(
    state: &AppState,
    key: &str,
    session_id: &str,
    cursor: Option<i64>,
    tx: &sse::ChunkSender,
    err: ProxyError,
) {
    state.threads.commit(key, session_id, cursor);
    tracing::info!(
        thread = %key,
        status = err.status.as_u16(),
        code = err.code,
        "turn failed"
    );
    send_stream_error(tx, err).await;
}

/// One live turn, from an accepted upstream response to `[DONE]`.
///
/// This used to be sixteen positional arguments to `stream_response`, which
/// made the interface as large as the implementation and forced every reader to
/// count arguments to learn what the turn carried. One struct instead, built by
/// `chat_completions` from `turn::prepare` plus the response it just opened.
struct LiveTurn {
    /// Held for the whole turn, including every correction turn.
    guard: tokio::sync::OwnedMutexGuard<()>,
    state: AppState,
    key: String,
    resp: reqwest::Response,
    slot: pool::RateTicket,
    /// Upstream body, re-sent for retries and correction turns.
    body: CompletionBody,
    id: String,
    model: String,
    created: u64,
    /// Flattened transcript, used to rebuild a session after a context limit.
    replay: String,
    can_recover: bool,
    include_usage: bool,
    started: std::time::Instant,
    specs: Vec<dialect::ToolSpec>,
    required: bool,
    parallel: bool,
    /// `false` collects the same chunks into one `chat.completion` body.
    stream: bool,
    /// Wire dialect; selects the streaming tool-call grammar.
    dialect: dialect::Dialect,
}

async fn stream_response(live: LiveTurn) -> Response {
    let LiveTurn {
        guard: turn,
        state,
        key,
        resp,
        slot,
        body,
        id,
        model,
        created,
        replay,
        can_recover,
        include_usage,
        started,
        specs,
        required,
        parallel,
        stream,
        dialect: wire,
    } = live;
    let (tx, rx) = tokio::sync::mpsc::channel::<sse::Chunk>(32);

    tokio::spawn(async move {
        // Held for the whole turn, including every correction turn, so another
        // request on this thread cannot start until this one has committed its
        // parent id. Dropped automatically when the task ends.
        let _turn = turn;
        // One snapshot for the whole turn: a SIGHUP mid-stream must not move
        // this turn's deadline or change its retry budget underneath it. New
        // turns pick up the new values; this one finishes on these.
        let turn_settings = state.settings.get();
        let mut attempt: u32 = 0;
        let mut context_recovered = false;
        let mut cursor_recovered = false;
        // How many commits this turn has already stepped back. Each rewind is
        // one cheap retry on the live session; only when the whole ring is
        // exhausted (or empty) is the session itself presumed gone and the
        // expensive replay path taken.
        let mut rewinds: usize = 0;
        let mut body = body;
        // The conversation cursor, ported from aiodeepseek's
        // `Conversation._parent_message_id`: the id of the NEWEST upstream
        // message. DeepSeek renders a message sent under an older parent as an
        // edited/alternate sibling, so every reply we generate — the main turn
        // and every correction turn — must advance this, and the value saved at
        // the end of the turn must be the last one, never the first.
        let mut session_id = body.chat_session_id.clone();
        let mut cursor = body.parent_message_id;
        let mut next = Some((resp, slot));

        // Opening chunk: OpenAI always sends the assistant role first.
        let opening = json!({
            "id": id, "object": "chat.completion.chunk",
            "created": created, "model": model,
            "choices": [{"index": 0, "delta": {"role": "assistant"}, "finish_reason": null}]
        });
        if tx
            .send(sse::Chunk::Data(opening.to_string()))
            .await
            .is_err()
        {
            return;
        }

        // Drain one upstream response, retrying while it is still safe.
        //
        // A fatal hint arrives BEFORE any content (aiodeepseek raises it out of
        // the stream iterator), so retrying the whole turn cannot duplicate
        // output the client already has.
        // The account is carried out of the loop, not read before it: only the
        // attempt that produced the final reply knows which one served it, and
        // a failover or retry in between must not be attributed to the first.
        let (pumped, account) = loop {
            let (resp, slot) = next.take().expect("attempt in flight");
            let account = slot.account.clone();
            // Hold the account's rate slot until the upstream body is drained.
            let idle =
                std::time::Duration::from_secs_f64(turn_settings.heartbeat_interval_secs);
            // The whole turn is bounded, like `asyncio.timeout(REQUEST_TIMEOUT)`.
            // A stalled upstream becomes a visible error instead of a hang.
            let budget = std::time::Duration::from_secs_f64(turn_settings.request_timeout_secs)
                .saturating_sub(started.elapsed());
            let pumped = match tokio::time::timeout(
                budget,
                pump(resp, &tx, &id, &model, created, idle, include_usage, wire),
            )
            .await
            {
                Ok(p) => p,
                Err(_) => {
                    state.stats.errors.fetch_add(1, Ordering::Relaxed);
                    tracing::error!(
                        thread = %key,
                        timeout_s = turn_settings.request_timeout_secs,
                        "turn timed out waiting for upstream"
                    );
                    fail_turn(
                        &state,
                        &key,
                        &session_id,
                        cursor,
                        &tx,
                        ProxyError::new(
                            "Request processing timed out",
                            axum::http::StatusCode::GATEWAY_TIMEOUT,
                            "request_timeout",
                        ),
                    )
                    .await;
                    return;
                }
            };
            // `pump` returns when the response body is dropped. Do not treat
            // that cancellation as a completed reply and start repair work.
            if tx.is_closed() {
                return;
            }
            drop(slot);

            // Advance the cursor only on a turn that actually succeeded.
            //
            // Port of aiodeepseek's `Conversation.ask_stream`, which assigns
            // `_parent_message_id = last_message_id` AFTER the stream loop
            // completes: when the stream raises (throttle, hint error) the
            // assignment never happens and the previous cursor is kept.
            //
            // Advancing on failure is what poisoned the cursor: a throttled
            // turn still carries a `response_message_id` for a message the
            // backend discarded, so the next turn parented to a message that
            // never existed and DeepSeek answered `{"code":26,"biz_msg":
            // "invalid message id"}`.
            let Some(hint) = pumped.hint.clone() else {
                if let Some(mid) = pumped.parent {
                    cursor = Some(mid);
                }
                break (pumped, account);
            };

            // Failed turn: keep the cursor we came in with.
            cursor = body.parent_message_id;

            if protocol::is_invalid_message_id(&hint) && !pumped.emitted && !cursor_recovered {
                // `{"code":26,"biz_msg":"invalid message id"}`: the saved
                // cursor does not exist upstream. Message ids are scoped to a
                // remote session, so this means the parent was deleted or the
                // session itself is gone. Clearing the cursor and resending the
                // DELTA would keep the old session id and open a fresh branch at
                // its root: every earlier turn is silently lost and the model
                // answers as if the conversation just started. Rebuild the
                // session and replay the whole transcript instead.
                cursor_recovered = true;
                attempt += 1;
                state.stats.retries.fetch_add(1, Ordering::Relaxed);

                // Cheapest recovery first: step the cursor back one commit and
                // resend the same delta, telling the model a reply was lost.
                //
                // This keeps the live remote session and the whole transcript
                // behind it, so it costs one turn instead of a full replay — and
                // a replay of a long thread is itself what trips the upstream
                // throttle, turning a recoverable turn into a 429 loop.
                let previous = if rewinds >= threads::REWIND_DEPTH {
                    None
                } else {
                    state.threads.rewind(&key)
                };
                if let Some(prev) = previous {
                    rewinds += 1;
                    tracing::warn!(
                        thread = %key,
                        prev,
                        rewinds,
                        hint = %hint,
                        "stale parent message id; rewinding one commit and continuing"
                    );
                    body.parent_message_id = Some(prev);
                    body.prompt = format!("{LOST_REPLY_NOTE}\n\n{}", body.prompt);
                    cursor = Some(prev);
                } else if can_recover {
                    tracing::warn!(
                        thread = %key,
                        hint = %hint,
                        "stale parent message id; rebuilding the session and replaying the transcript"
                    );
                    match rebuild_session(&state, &key, session::RebuildReason::StaleCursor).await {
                        Ok(fresh) => {
                            let thinking = body.thinking_enabled;
                            let search = body.search_enabled;
                            body = completion_body(
                                &fresh,
                                &replay,
                                thinking,
                                search,
                                &body.ref_file_ids,
                                &body.model_type,
                            );
                            session_id = fresh.session_id.clone();
                            cursor = None;
                        }
                        Err(_) => {
                            fail_turn(
                                &state,
                                &key,
                                &session_id,
                                cursor,
                                &tx,
                                ProxyError::new(
                                    "Backend request failed",
                                    axum::http::StatusCode::BAD_GATEWAY,
                                    "backend_error",
                                ),
                            )
                            .await;
                            return;
                        }
                    }
                } else {
                    // Nothing to replay (first turn): dropping the cursor is
                    // enough to recover.
                    tracing::warn!(
                        thread = %key,
                        hint = %hint,
                        "stale parent message id; retrying with a cleared cursor"
                    );
                    cursor = None;
                    body.parent_message_id = None;
                }
            } else if protocol::is_context_limit(&hint) {
                // Context overflow: rebuild the remote session and replay once.
                if context_recovered || !can_recover || pumped.emitted {
                    state.stats.errors.fetch_add(1, Ordering::Relaxed);
                    tracing::error!(thread = %key, "upstream hint error: {hint}");
                    fail_turn(
                        &state,
                        &key,
                        &session_id,
                        cursor,
                        &tx,
                        ProxyError::new(
                            hint.clone(),
                            axum::http::StatusCode::BAD_GATEWAY,
                            "context_length_exceeded",
                        ),
                    )
                    .await;
                    return;
                }
                context_recovered = true;
                match rebuild_session(&state, &key, session::RebuildReason::ContextLimit).await {
                    Ok(fresh) => {
                        let thinking = body.thinking_enabled;
                        let search = body.search_enabled;
                        body = completion_body(
                            &fresh,
                            &replay,
                            thinking,
                            search,
                            &body.ref_file_ids,
                            &body.model_type,
                        );
                        // The rebuilt session is a different conversation: point
                        // the cursor at it, or the next turn parents into the
                        // session we just abandoned.
                        session_id = fresh.session_id.clone();
                        cursor = None;
                    }
                    Err(_) => {
                        fail_turn(
                            &state,
                            &key,
                            &session_id,
                            cursor,
                            &tx,
                            ProxyError::new(
                                "Backend request failed",
                                axum::http::StatusCode::BAD_GATEWAY,
                                "backend_error",
                            ),
                        )
                        .await;
                        return;
                    }
                }
            } else if pool::is_rate_limit(&hint) && !pumped.emitted {
                let wait = backoff_delay(&turn_settings, attempt);
                if attempt >= turn_settings.rate_limit_max_retries {
                    state.stats.errors.fetch_add(1, Ordering::Relaxed);
                    // Flag the thread so `deepseek-rs rebind` can find it, most
                    // recent first, even after a restart. The account, not the
                    // thread, is what was throttled; this records the victim.
                    if let Err(e) = state.store.mark_throttled(&key) {
                        tracing::warn!(thread = %key, error = %e, "could not record throttle");
                    }
                    tracing::error!(
                        thread = %key,
                        retries = attempt,
                        "upstream still throttled: {hint}"
                    );
                    fail_turn(
                        &state,
                        &key,
                        &session_id,
                        cursor,
                        &tx,
                        error_for(
                            &state,
                            &key,
                            CompletionError::RateLimited {
                                retry_after_s: wait.ceil() as u64,
                            },
                        )
                        .await,
                    )
                    .await;
                    return;
                }
                attempt += 1;
                state.stats.retries.fetch_add(1, Ordering::Relaxed);
                tracing::info!(
                    thread = %key,
                    attempt,
                    retry_delay_s = wait,
                    hint = %hint,
                    "retrying after backend throttle"
                );
                state
                    .pool()
                    .route(&key)
                    .await
                    .cool_down(std::time::Duration::from_secs_f64(wait))
                    .await;
            } else {
                state.stats.errors.fetch_add(1, Ordering::Relaxed);
                tracing::error!(thread = %key, "upstream hint error: {hint}");
                let login_dead = pool::is_login_dead(&hint);
                let err = if login_dead {
                    CompletionError::LoginDead(hint.clone())
                } else {
                    CompletionError::Other(hint.clone())
                };
                fail_turn(
                    &state,
                    &key,
                    &session_id,
                    cursor,
                    &tx,
                    error_for(&state, &key, err).await,
                )
                .await;
                return;
            }

            match completion_with_retry(&state, &key, &body).await {
                Ok(v) => next = Some(v),
                Err(e) => {
                    fail_turn(
                        &state,
                        &key,
                        &session_id,
                        cursor,
                        &tx,
                        error_for(&state, &key, e).await,
                    )
                    .await;
                    return;
                }
            }
        };

        let Pumped {
            parent,
            mut filter,
            text,
            prose,
            prompt_tokens,
            ..
        } = pumped;

        // Truly empty = the upstream sent no text at all, no call, no hint.
        //
        // Do NOT test `emitted`: the stream filter holds text back while it
        // waits for a closing tag, so a perfectly normal reply that ends inside
        // a partial `<invoke` has `emitted == false`. Testing `emitted` here
        // (an earlier version did) failed those turns as "empty response" —
        // behaviour the Python proxy never had, because it flushes the buffer
        // and finishes normally.
        if text.trim().is_empty() && filter.calls.is_empty() {
            state.stats.errors.fetch_add(1, Ordering::Relaxed);
            tracing::error!(
                thread = %key,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "upstream closed the stream without content or a hint"
            );
            fail_turn(
                &state,
                &key,
                &session_id,
                cursor,
                &tx,
                ProxyError::new(
                    "Backend returned an empty response",
                    axum::http::StatusCode::BAD_GATEWAY,
                    "empty_response",
                ),
            )
            .await;
            return;
        }

        // Validate what the model produced and run correction turns, exactly
        // like `Recovery` does after the stream is drained. The streamed calls
        // were parsed but NOT yet validated, so re-run them through the policy
        // here: an unknown tool or bad arguments must trigger a repair turn
        // rather than reach the client.
        // `scan(&text)` replays the whole reply in order, yielding text, valid
        // calls and malformed blocks. It supersedes `filter.calls` (parsed from
        // the same text) and the separate `extract_malformed` pass, so the
        // policy sees each event exactly once.
        filter.calls.clear();
        let mut recovery = Recovery::new(specs, required, parallel, wire);
        for event in wire.scan(&text) {
            match recovery.accept(event) {
                Ok(_) => {}
                Err(e) => {
                    state.stats.errors.fetch_add(1, Ordering::Relaxed);
                    fail_turn(&state, &key, &session_id, cursor, &tx, e).await;
                    return;
                }
            }
        }
        recovery.require_call();

        // Chain correction turns after the reply we just received: the cursor
        // already points at it, so the repair continues the conversation instead
        // of branching off the pre-turn message (DeepSeek renders siblings as
        // edited/alternate messages).

        let max = turn_settings.tool_repair_max_retries;
        // `begin_repair` clears the pending failures, so a repair that dies on
        // the wire must be remembered separately: otherwise the loop exits with
        // nothing pending and the turn is delivered as an empty success.
        let mut repair_aborted = false;
        for attempt in 1..=max {
            if tx.is_closed() {
                return;
            }
            if !recovery.has_pending() {
                break;
            }
            let prompt = recovery.begin_repair(attempt as usize);
            let expected = recovery.expected;
            tracing::warn!(thread = %key, attempt, expected, "tool repair");

            // The repair goes to whichever session is live right now, parented
            // to the newest message — including a rebuilt one.
            let turn_state = ThreadState {
                session_id: session_id.clone(),
                parent_message_id: cursor,
                prev_parents: Vec::new(),
                exchanges: 0,
            };
            // Thinking on every repair turn.
            //
            // The prompt asks the model to check the block's shape before
            // emitting it, which is a deliberation the request has to allow:
            // with thinking off there is no scratch space to do it in, so the
            // instruction is unfollowable. That was inherited from Python
            // (`turn.generate(repair_prompt, stream=False)`), not chosen — and
            // it is the likely reason a model could fail the same way five
            // times while a single nudge in the chat, on a turn that *did*
            // think, fixed it.
            //
            // Cost: thinking tokens on a non-streaming call, so the repair
            // waits for the whole reply. That is the right trade — a repair
            // that works costs one turn, a repair that fails costs the turn
            // plus every retry after it.
            let body = completion_body(&turn_state, &prompt, true, false, &[], &body.model_type);
            let completion = match until_client_disconnect(
                &tx,
                completion_with_retry(&state, &key, &body),
            )
            .await
            {
                Some(result) => result,
                None => return,
            };
            let (resp, _slot) = match completion {
                Ok(v) => v,
                Err(e) => {
                    tracing::error!("thread {key}: repair completion failed: {e}");
                    repair_aborted = true;
                    break;
                }
            };
            let raw = match until_client_disconnect(&tx, resp.text()).await {
                Some(Ok(text)) => text,
                Some(Err(e)) => {
                    tracing::error!("thread {key}: repair body failed: {e}");
                    repair_aborted = true;
                    break;
                }
                None => return,
            };
            let (_, repair_text, repair_mid) = route_all(&raw);
            // The correction answer is now the newest upstream message, so it —
            // not the malformed reply it fixed — becomes the parent of the
            // client's next turn. Saving `parent` here is what used to make
            // DeepSeek draw the next message as an edit of the broken answer.
            if let Some(mid) = repair_mid {
                cursor = Some(mid);
            }
            for event in wire.scan(&repair_text) {
                match recovery.accept(event) {
                    Ok(_) => {}
                    Err(e) => {
                        state.stats.errors.fetch_add(1, Ordering::Relaxed);
                        fail_turn(&state, &key, &session_id, cursor, &tx, e).await;
                        return;
                    }
                }
            }
            recovery.end_repair();
            tracing::debug!(
                thread = %key,
                attempt,
                remaining = recovery.pending.len(),
                "tool repair settled"
            );
        }

        // Python raises here rather than returning a turn with no usable calls.
        if recovery.has_pending() || repair_aborted {
            state.stats.errors.fetch_add(1, Ordering::Relaxed);
            fail_turn(
                &state,
                &key,
                &session_id,
                cursor,
                &tx,
                ProxyError::new(
                    "Could not recover a valid tool response",
                    axum::http::StatusCode::BAD_GATEWAY,
                    "tool_repair_failed",
                ),
            )
            .await;
            return;
        }
        filter.calls = recovery.calls;

        // Flush any held-back prose, then emit tool calls (if any).
        let tail = filter.finish();
        if !tail.is_empty() {
            let chunk = json!({
                "id": id, "object": "chat.completion.chunk",
                "created": created, "model": model,
                "choices": [{"index": 0, "delta": {"content": tail}, "finish_reason": null}]
            });
            let _ = tx.send(sse::Chunk::Data(chunk.to_string())).await;
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
                        "function": { "name": c.name, "arguments": c.arguments }
                    })
                })
                .collect();
            let chunk = json!({
                "id": id, "object": "chat.completion.chunk",
                "created": created, "model": model,
                "choices": [{"index": 0, "delta": {"tool_calls": calls}, "finish_reason": null}]
            });
            let _ = tx.send(sse::Chunk::Data(chunk.to_string())).await;
            "tool_calls"
        };

        state
            .stats
            .tool_calls
            .fetch_add(filter.calls.len() as u64, Ordering::Relaxed);
        state.stats.ok.fetch_add(1, Ordering::Relaxed);
        // One line per completed turn, with everything the PLAN asks for. The
        // request id arrives through the `turn` span, so it is not repeated
        // here; `account` comes from the rate ticket that served the turn.
        let turn_usage = usage_block(prompt_tokens, &prose, &filter.calls);
        tracing::info!(
            thread = %key,
            account = %short_account(&account),
            status = 200,
            finish_reason,
            tool_calls = filter.calls.len(),
            ms = started.elapsed().as_millis() as u64,
            prompt_tokens = turn_usage["prompt_tokens"].as_u64().unwrap_or(0),
            completion_tokens = turn_usage["completion_tokens"].as_u64().unwrap_or(0),
            parent = ?parent,
            "turn done"
        );

        let done = json!({
            "id": id,
            "object": "chat.completion.chunk",
            "created": created,
            "model": model,
            "choices": [{"index": 0, "delta": {}, "finish_reason": finish_reason}]
        });
        let _ = tx.send(sse::Chunk::Data(done.to_string())).await;
        // With `include_usage`, OpenAI sends one last chunk with no choices and
        // the token counts, immediately before `[DONE]`.
        if include_usage {
            let usage = usage_block(prompt_tokens, &prose, &filter.calls);
            let chunk = json!({
                "id": id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model,
                "choices": [],
                "usage": usage
            });
            let _ = tx.send(sse::Chunk::Data(chunk.to_string())).await;
        }
        let _ = tx.send(sse::Chunk::Data("[DONE]".into())).await;

        state.threads.commit(&key, &session_id, cursor);
    });

    if stream {
        return Sse::new(sse::sse_stream(rx)).into_response();
    }
    collect_completion(rx).await
}

/// Cancel turn work as soon as the client drops the response body.
async fn until_client_disconnect<F: std::future::Future>(
    tx: &sse::ChunkSender,
    future: F,
) -> Option<F::Output> {
    tokio::select! {
        biased;
        _ = tx.closed() => None,
        result = future => Some(result),
    }
}

/// Drain a turn into one `chat.completion` body.
///
/// The turn itself is identical to the streaming one — same retries, same
/// tool repair, same cursor handling — so the only difference is transport:
/// concatenate the deltas instead of forwarding them. `stream: false` used to
/// get an SSE body, which no OpenAI SDK can parse.
async fn collect_completion(mut rx: sse::ChunkReceiver) -> Response {
    let mut id = String::new();
    let mut model = String::new();
    let mut created = 0u64;
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut calls: Vec<Value> = Vec::new();
    let mut finish_reason = "stop";
    let mut usage: Option<Value> = None;

    while let Some(chunk) = rx.recv().await {
        let sse::Chunk::Data(data) = chunk else {
            if let sse::Chunk::Error(status, payload) = chunk {
                return (status, Json(payload)).into_response();
            }
            continue;
        };
        if data == "[DONE]" {
            break;
        }
        let Ok(value) = serde_json::from_str::<Value>(&data) else {
            continue;
        };
        if let Some(v) = value.get("id").and_then(Value::as_str) {
            id = v.to_string();
        }
        if let Some(v) = value.get("model").and_then(Value::as_str) {
            model = v.to_string();
        }
        if let Some(v) = value.get("created").and_then(Value::as_u64) {
            created = v;
        }
        // The trailing usage chunk carries no choices.
        if let Some(v) = value.get("usage").filter(|v| !v.is_null()) {
            usage = Some(v.clone());
        }
        let Some(choice) = value.pointer("/choices/0") else {
            continue;
        };
        if let Some(delta) = choice.get("delta") {
            if let Some(text) = delta.get("content").and_then(Value::as_str) {
                content.push_str(text);
            }
            if let Some(text) = delta.get("reasoning_content").and_then(Value::as_str) {
                reasoning.push_str(text);
            }
            if let Some(list) = delta.get("tool_calls").and_then(Value::as_array) {
                calls.extend(list.iter().cloned());
            }
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            finish_reason = match reason {
                "tool_calls" => "tool_calls",
                "length" => "length",
                _ => "stop",
            };
        }
    }

    let mut message = json!({"role": "assistant", "content": content});
    if !reasoning.is_empty() {
        message["reasoning_content"] = json!(reasoning);
    }
    if !calls.is_empty() {
        message["tool_calls"] = json!(calls);
    }
    let mut body = json!({
        "id": id,
        "object": "chat.completion",
        "created": created,
        "model": model,
        "choices": [{"index": 0, "message": message, "finish_reason": finish_reason}],
    });
    if let Some(usage) = usage {
        body["usage"] = usage;
    }
    Json(body).into_response()
}

/// Port of `ChatService.validate()`'s request-shape checks (the pydantic model
/// plus the tool-schema pass). Everything here is a client error: the handler
/// returns it before touching the pool or the thread lock.
fn validate_request(req: &ChatRequest) -> Result<(), ProxyError> {
    // `model` is optional here: the handler defaults it from config, which the
    // Python proxy does not allow but this port documents deliberately.
    if req.messages.is_empty() {
        return Err(invalid("messages", "List should have at least 1 item"));
    }
    if let Some(id) = &req.thread_id {
        if id.is_empty() || id.chars().count() > 256 {
            return Err(invalid("thread_id", "Invalid thread id"));
        }
    }
    // `tool_choice`: only auto/none/required or a named-function object.
    if let Some(choice) = &req.tool_choice {
        let ok = match choice {
            Value::String(s) => matches!(s.as_str(), "auto" | "none" | "required"),
            Value::Object(obj) => obj
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .is_some_and(|n| !n.is_empty()),
            _ => false,
        };
        if !ok {
            return Err(invalid(
                "tool_choice",
                "Input should be a valid tool choice",
            ));
        }
    }
    for (i, m) in req.messages.iter().enumerate() {
        let at = |field: &str| format!("messages.{i}.{field}");
        if m.role == "tool" && m.tool_call_id.as_deref().unwrap_or_default().is_empty() {
            return Err(invalid(
                &at("tool_call_id"),
                "Tool messages require tool_call_id",
            ));
        }
        // Assistant messages may carry only tool_calls; every other role needs content.
        let content_missing = match &m.content {
            None => true,
            Some(Value::String(s)) => s.is_empty(),
            Some(Value::Array(parts)) => parts.is_empty(),
            Some(_) => false,
        };
        if content_missing && !(m.role == "assistant" && !m.tool_calls.is_empty()) {
            return Err(invalid(
                &at("content"),
                "Message requires content or assistant tool_calls",
            ));
        }
        if let Some(Value::Array(parts)) = &m.content {
            for (j, part) in parts.iter().enumerate() {
                let ok = match part.get("type").and_then(|t| t.as_str()) {
                    Some("text") => part.get("text").and_then(|t| t.as_str()).is_some(),
                    Some("image_url") => {
                        m.role == "user"
                            && part
                                .get("image_url")
                                .and_then(|i| i.get("url"))
                                .and_then(|u| u.as_str())
                                .is_some()
                    }
                    _ => false,
                };
                if !ok {
                    return Err(invalid(
                        &at(&format!("content.{j}")),
                        "Unsupported or invalid message content part",
                    ));
                }
            }
        }
    }
    for tool in &req.tools {
        let Some(func) = tool.get("function") else {
            return Err(invalid("tools", "Invalid or unsupported tool schema"));
        };
        let name = func
            .get("name")
            .and_then(|n| n.as_str())
            .unwrap_or_default();
        if name.is_empty() || name.chars().count() > 128 {
            return Err(invalid("tools", "Invalid or unsupported tool schema"));
        }
        let parameters = func.get("parameters").cloned().unwrap_or_else(|| json!({}));
        if !parameters.is_object() {
            return Err(invalid("tools", "Invalid or unsupported tool schema"));
        }
        // Python rejects the schema itself, and refuses remote `$ref`s so no
        // network fetch can happen during validation.
        if has_external_ref(&parameters) || jsonschema::validator_for(&parameters).is_err() {
            return Err(invalid("tools", "Invalid or unsupported tool schema"));
        }
    }
    Ok(())
}

fn invalid(param: &str, message: &str) -> ProxyError {
    ProxyError::new(
        message,
        axum::http::StatusCode::BAD_REQUEST,
        "invalid_request",
    )
    .with_param(param)
}

/// True when the schema contains a `$ref`/`$dynamicRef` that points outside the
/// document. Port of the recursive check in `ChatService.validate()`.
fn has_external_ref(node: &Value) -> bool {
    match node {
        Value::Object(map) => map.iter().any(|(k, v)| {
            (k == "$ref" || k == "$dynamicRef")
                .then(|| !v.as_str().unwrap_or_default().starts_with('#'))
                .unwrap_or(false)
                || has_external_ref(v)
        }),
        Value::Array(items) => items.iter().any(has_external_ref),
        _ => false,
    }
}

/// `ChatService.validate()`'s supported-model set, case-insensitive.
fn is_supported_model(model: &str) -> bool {
    matches!(
        model.to_ascii_uppercase().as_str(),
        "DEFAULT" | "EXPERT" | "VISION" | "DEEPSEEK-CHAT" | "DEEPSEEK-V3"
    )
}

/// Backend failure. Port of `error_for()`'s fallback: the detail is logged and
/// the client gets the generic OpenAI envelope, never the raw message.
fn backend_error(msg: &str) -> Response {
    tracing::error!(error = %msg, "backend request failed");
    ProxyError::new(
        "Backend request failed",
        axum::http::StatusCode::BAD_GATEWAY,
        "backend_error",
    )
    .into_response()
}

/// Build a connected account pool from resolved config.
pub(crate) async fn connect_pool(
    cfg: &config::Config,
    store: &store::Store,
    accounts: Vec<pool::AccountSpec>,
) -> Result<Arc<pool::Pool>, Box<dyn std::error::Error>> {
    let pool_config = pool::PoolConfig::from_config(cfg);
    let p = pool::Pool::connect(accounts, pool_config, store.clone()).await?;
    Ok(Arc::new(p))
}

/// Build the HTTP surface. Extracted from `main()` so integration tests can
/// drive the real router over a real socket without a DeepSeek account.
fn build_router(state: AppState) -> Router {
    // axum buffers the whole body before `Json<ChatRequest>` runs and refuses
    // anything over 2 MiB by default. A single 10 MiB image is ~13.3 MiB of
    // base64, so the default rejects perfectly valid turns with
    // "Failed to buffer the request body: length limit exceeded". The cap is
    // derived from IMAGE_MAX_BYTES, not a magic number. Floor is 128 MiB so
    // large multi-image turns and big tool transcripts get through.
    // Read once, at router build time: `DefaultBodyLimit` is baked into the
    // layer, so changing IMAGE_MAX_BYTES needs a restart. The reload path logs
    // that, like host/port.
    let body_limit = state
        .settings
        .get()
        .image_max_bytes
        .saturating_mul(2)
        .max(128 << 20);

    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/stats", get(stats))
        // Management surface for `deepseek-rs rebind`. Both handlers call
        // `authorize`, so a valid API key is required to hop accounts.
        .route("/admin/throttled", get(admin_throttled))
        .route("/admin/rebind", post(admin_rebind))
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(chat_completions))
        // Honest refusals instead of 404: a client probing capabilities should
        // learn that this proxy does not do embeddings, not that the route is
        // missing. One handler, one envelope.
        .route("/v1/completions", post(not_supported))
        .route("/v1/embeddings", post(not_supported))
        .route("/v1/images/generations", post(not_supported))
        .route("/v1/images/edits", post(not_supported))
        .route("/v1/audio/speech", post(not_supported))
        .route("/v1/audio/transcriptions", post(not_supported))
        .route("/v1/audio/translations", post(not_supported))
        .fallback(fallback)
        .method_not_allowed_fallback(method_not_allowed)
        .with_state(state)
        // Must come AFTER the routes: `Router::layer` only wraps routes that
        // already exist, so placing it earlier silently does nothing.
        .layer(axum::extract::DefaultBodyLimit::max(body_limit))
        // Outermost: every response, including the 404/405 fallbacks, carries
        // the id, and every event logged while handling the request has it.
        .layer(axum::middleware::from_fn(request_id))
        // Before `request_id` runs, so even a preflight gets both headers.
        .layer(axum::middleware::from_fn(cors))
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

    // `--port` / `--host` on `serve` (forwarded by `start`) beat the
    // environment and `config.toml`; with neither, the resolved config's
    // default (0.0.0.0:9174) stands.
    let (host, port) = cli::bind_overrides(&args)?;
    let cfg = config::Config::load_with(host, port)?;
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
        version = env!("CARGO_PKG_VERSION"),
        built = %build_id(),
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

    let settings = Settings::from_config(&cfg);
    tracing::info!(
        db_path = %cfg.db_path,
        model = %settings.model_name,
        dialect = cfg.dialect.name(),
        thinking = settings.thinking_default,
        search = settings.search_enabled,
        "settings loaded"
    );

    let state = AppState {
        pool: Arc::new(std::sync::RwLock::new(pool)),
        settings: Arc::new(SettingsHandle::new(cfg.clone())),
        threads: Arc::new(threads::Threads::new(Arc::new(
            threads::SqlitePersist::new(store.clone()),
        ))),
        store,
        stats: Arc::new(Stats::new()),
    };

    // axum buffers the whole body before `Json<ChatRequest>` runs and refuses
    // anything over 2 MiB by default. A single 10 MiB image is ~13.3 MiB of
    // base64, so the default rejects perfectly valid turns with
    // "Failed to buffer the request body: length limit exceeded". The cap is
    // derived from IMAGE_MAX_BYTES, not a magic number. Floor is 128 MiB so
    // large multi-image turns and big tool transcripts get through.
    let app = build_router(state.clone());

    let addr = format!("{}:{}", cfg.host, cfg.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("listening on http://{addr}");

    // Graceful shutdown with a deadline. axum's own graceful shutdown waits
    // for in-flight requests forever, so one stalled SSE stream used to keep
    // the process alive until the CLI gave up and sent SIGKILL. Readiness flips
    // first (new turns go elsewhere), then the drain is bounded: whatever is
    // still running when the grace period ends is abandoned, because holding
    // the process hostage is worse than truncating one stream.
    // Hot reload: SIGHUP re-reads the config. Spawned, never awaited here.
    tokio::spawn(reload_loop(state.clone()));

    let grace = std::time::Duration::from_secs(cfg.shutdown_grace_secs);
    let stats_for_signal = state.stats.clone();
    // The drain deadline must start when the signal arrives, not when the
    // listener binds. Wrapping the whole `serve` future in `timeout(grace, ..)`
    // started that clock at startup, so the process exited exactly
    // `shutdown_grace_secs` after boot, with or without a signal — a server
    // that served for 20 seconds and then stopped itself.
    let (stopping_tx, stopping_rx) = tokio::sync::oneshot::channel();
    let serve = async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                shutdown_signal().await;
                stats_for_signal
                    .shutting_down
                    .store(true, Ordering::Relaxed);
                let _ = stopping_tx.send(());
            })
            .await
    };
    tokio::pin!(serve);
    let drained = tokio::select! {
        r = &mut serve => {
            r?;
            true
        }
        // Arms only once the signal fires, then bounds the drain.
        _ = async {
            let _ = stopping_rx.await;
            tokio::time::sleep(grace).await;
        } => false,
    };
    if !drained {
        tracing::warn!(
            grace_s = cfg.shutdown_grace_secs,
            "grace period elapsed with requests still in flight; exiting"
        );
    }
    tracing::info!("stopped");
    Ok(())
}

/// Serializes reloads: two SIGHUPs close together must not interleave.
static RELOAD_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Re-read `config.toml` and install it, without dropping a running turn.
///
/// A turn already in flight keeps the snapshot it took, so nothing here can
/// move a deadline mid-stream. Two failure modes are handled explicitly: a
/// config that does not parse leaves the running one untouched (and says so in
/// `/stats`), and a key that cannot change while bound is reported rather than
/// silently ignored.
async fn reload(state: &AppState) {
    let _guard = RELOAD_LOCK.lock().await;
    if state.stats.shutting_down.load(Ordering::Relaxed) {
        tracing::info!("SIGHUP ignored: shutting down");
        return;
    }
    tracing::info!("reloading config");

    let fresh = match config::Config::load() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "config reload failed; keeping the running config");
            state.settings.note_error(e);
            return;
        }
    };

    // Reported before the swap, because `replace` moves `fresh`.
    for key in state.settings.restart_only_changes(&fresh) {
        tracing::warn!(key, "not reloadable; restart required");
    }

    // Accounts and pool tunables are not part of `Settings`: they live in the
    // pool, which can only be changed by rebuilding it. Decide that here, while
    // both the old pool and the new config are in hand.
    let old_pool = state.pool();
    let pool_changed = old_pool.config() != &pool::PoolConfig::from_config(&fresh);
    let wanted: Vec<String> = cli::load_accounts(&state.store)
        .into_iter()
        .map(|a| a.email)
        .collect();
    let accounts_changed = wanted != old_pool.configured_emails();

    let dialect = fresh.dialect.name();
    let generation = state.settings.replace(fresh.clone());
    tracing::info!(generation, dialect, "config reloaded");

    if pool_changed || accounts_changed {
        rebuild_pool(state, fresh, old_pool).await;
    }
}

/// Rebuild the pool off the request path.
///
/// Account changes require a login, which is network I/O and must never delay
/// the settings swap that already happened. `AppState::set_pool` is the only
/// mutation, so a turn that is already running finishes on the old pool while
/// new turns get the new one. Thread bindings live in SQLite and are re-read by
/// `Pool::route`, so no conversation is stranded by the swap.
async fn rebuild_pool(state: &AppState, cfg: config::Config, old: Arc<pool::Pool>) {
    let accounts = cli::load_accounts(&state.store);
    tracing::info!(accounts = accounts.len(), "reloading account pool");
    match connect_pool(&cfg, &state.store, accounts).await {
        Ok(fresh) => {
            let before = old.configured_emails().to_vec();
            let after = fresh.configured_emails().to_vec();
            for email in after.iter().filter(|e| !before.contains(e)) {
                tracing::info!(account = %email, "account added by reload");
            }
            for email in before.iter().filter(|e| !after.contains(e)) {
                tracing::warn!(account = %email, "account removed by reload");
            }
            state.set_pool(fresh);
            tracing::info!("account pool reloaded");
        }
        Err(e) => {
            // The running pool stays: an unbuildable pool must not take the
            // proxy down, exactly like an unparseable config.
            tracing::error!(error = %e, "pool reload failed; keeping the running pool");
            state.settings.note_error(e.to_string());
        }
    }
}

/// Apply SIGHUP forever. Separate from [`shutdown_signal`] so the two never
/// compete for the same signal.
async fn reload_loop(state: AppState) {
    use tokio::signal::unix::{signal, SignalKind};
    let Ok(mut hup) = signal(SignalKind::hangup()) else {
        tracing::warn!("SIGHUP unavailable; hot reload is disabled");
        return;
    };
    loop {
        hup.recv().await;
        reload(&state).await;
    }
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

    #[tokio::test]
    async fn disconnect_cancels_inflight_repair_future() {
        use std::sync::atomic::AtomicBool;

        struct DropFlag(Arc<AtomicBool>);
        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Relaxed);
            }
        }

        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let dropped = Arc::new(AtomicBool::new(false));
        let drop_flag = Arc::clone(&dropped);
        let repair = async move {
            let _drop_flag = DropFlag(drop_flag);
            let _ = started_tx.send(());
            std::future::pending::<()>().await;
        };
        let waiter = tokio::spawn(async move { until_client_disconnect(&tx, repair).await });

        started_rx.await.unwrap();
        drop(rx);

        assert_eq!(waiter.await.unwrap(), None);
        assert!(dropped.load(Ordering::Relaxed), "repair future was not cancelled");
    }

    fn msg_with(role: &str, content: &str) -> Msg {
        Msg {
            role: role.into(),
            content: Some(serde_json::json!(content)),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    fn thread(exchanges: usize) -> ThreadState {
        ThreadState {
            session_id: "s".into(),
            parent_message_id: None,
            prev_parents: Vec::new(),
            exchanges,
        }
    }

    fn request(messages: Vec<Msg>, tools: Vec<serde_json::Value>) -> ChatRequest {
        ChatRequest {
            model: None,
            thread_id: None,
            messages,
            stream: false,
            reasoning_effort: None,
            thinking: None,
            reasoning: None,
            tools,
            tool_choice: None,
            parallel_tool_calls: true,
            stream_options: None,
        }
    }

    fn bash_tool() -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": {"name": "bash", "description": "run a command"}
        })
    }

    fn tool_with_schema() -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "bash",
                "description": "run a command",
                "parameters": {
                    "type": "object",
                    "properties": {"command": {"type": "string"}},
                    "required": ["command"]
                }
            }
        })
    }

    /// One delta feeds both the prompt and the image intake, so they can never
    /// disagree about which turn they are describing (Python: `get_new_messages`).
    #[test]
    fn delta_is_everything_after_the_last_assistant() {
        let messages = vec![
            msg_with("user", "old"),
            msg_with("assistant", "reply"),
            msg_with("user", "new"),
        ];
        let delta = delta_messages(&messages);
        assert_eq!(delta.len(), 1);
        assert_eq!(content_text(&delta[0].content), "new");

        let (prompt, _) = build_prompts(&request(messages, vec![]), Some(&thread(1)), "", None);
        assert_eq!(prompt, "User: new");
    }

    /// Tool round trip: the delta carries the assistant's call and the tool
    /// results, and the continuation line is appended once.
    #[test]
    fn continuation_renders_whole_delta_and_continues_after_tool() {
        let mut call = msg_with("assistant", "");
        call.tool_calls = vec![serde_json::json!({
            "id": "call_0",
            "type": "function",
            "function": {"name": "bash", "arguments": "{}"}
        })];
        let mut result = msg_with("tool", "output");
        result.tool_call_id = Some("call_0".into());
        let messages = vec![msg_with("user", "run it"), call, result];

        let (prompt, _) = build_prompts(&request(messages, vec![]), Some(&thread(1)), "", None);
        assert!(prompt.contains("run it") || prompt.starts_with("Tool result"));
        assert!(prompt.contains("Tool result (id=call_0):\noutput"));
        assert_eq!(prompt.matches(CONTINUE_AFTER_TOOL).count(), 1);
    }

    #[test]
    fn validation_rejects_tool_message_without_id() {
        let mut m = msg_with("tool", "output");
        m.tool_call_id = None;
        let req = request(vec![m], vec![]);
        let e = validate_request(&req).unwrap_err();
        assert_eq!(e.status, axum::http::StatusCode::BAD_REQUEST);
        assert_eq!(e.code, "invalid_request");
        assert_eq!(e.param.as_deref(), Some("messages.0.tool_call_id"));
    }

    #[test]
    fn settings_handle_swaps_and_reports_generation() {
        let mut cfg = config::Config::load().expect("config loads");
        cfg.heartbeat_interval_secs = 10.0;
        let handle = SettingsHandle::new(cfg.clone());

        assert_eq!(handle.get().heartbeat_interval_secs, 10.0);
        assert_eq!(handle.snapshot().0, 1);

        cfg.heartbeat_interval_secs = 3.0;
        assert_eq!(handle.replace(cfg), 2);
        assert_eq!(handle.get().heartbeat_interval_secs, 3.0);
        assert_eq!(handle.snapshot().0, 2);
        assert_eq!(handle.snapshot().2, None);
    }

    #[test]
    fn a_failed_reload_keeps_the_running_config_and_records_the_error() {
        let cfg = config::Config::load().expect("config loads");
        let before = cfg.model.clone();
        let handle = SettingsHandle::new(cfg);

        handle.note_error("bad toml".into());

        // The running config is untouched: a parse error must not brick a
        // server that is already serving.
        assert_eq!(handle.get().model_name, before);
        assert_eq!(handle.snapshot().0, 1);
        assert_eq!(handle.snapshot().2.as_deref(), Some("bad toml"));
    }

    #[test]
    fn restart_only_keys_are_reported_not_applied() {
        let a = config::Config::load().expect("config loads");
        let mut b = config::Config::load().expect("config loads");
        assert!(restart_only_keys(&a, &b).is_empty());

        b.port = a.port.wrapping_add(1);
        b.db_path = format!("{}.other", a.db_path);
        b.image_max_bytes = a.image_max_bytes + 1;
        let changed = restart_only_keys(&a, &b);
        assert!(changed.contains(&"host/port"), "{changed:?}");
        assert!(changed.contains(&"db_path"), "{changed:?}");
        assert!(changed.contains(&"image_max_bytes"), "{changed:?}");

        // A live key is not in the list: it is applied, not reported.
        b.request_timeout_secs = a.request_timeout_secs + 5.0;
        assert!(!restart_only_keys(&a, &b).contains(&"request_timeout_secs"));
    }

    #[test]
    fn validation_rejects_empty_messages_and_unknown_tool_choice() {
        let req = request(vec![], vec![]);
        assert_eq!(validate_request(&req).unwrap_err().code, "invalid_request");

        let mut req = request(vec![msg_with("user", "hi")], vec![]);
        req.tool_choice = Some(serde_json::json!("banana"));
        assert_eq!(
            validate_request(&req).unwrap_err().param.as_deref(),
            Some("tool_choice")
        );
        for good in ["auto", "none", "required"] {
            req.tool_choice = Some(serde_json::json!(good));
            assert!(validate_request(&req).is_ok(), "{good} must be accepted");
        }
    }

    #[test]
    fn validation_rejects_invalid_tool_schema_and_external_ref() {
        let mut bad = tool_with_schema();
        bad["function"]["parameters"] = serde_json::json!({"type": 123});
        let req = request(vec![msg_with("user", "hi")], vec![bad]);
        assert_eq!(
            validate_request(&req).unwrap_err().param.as_deref(),
            Some("tools")
        );

        let mut remote = tool_with_schema();
        remote["function"]["parameters"] = serde_json::json!({
            "type": "object",
            "properties": {"a": {"$ref": "https://example.com/schema.json"}}
        });
        let req = request(vec![msg_with("user", "hi")], vec![remote]);
        assert_eq!(
            validate_request(&req).unwrap_err().param.as_deref(),
            Some("tools")
        );

        let req = request(vec![msg_with("user", "hi")], vec![tool_with_schema()]);
        assert!(validate_request(&req).is_ok());
    }

    #[test]
    fn validation_accepts_assistant_tool_calls_without_content() {
        let mut m = msg_with("assistant", "");
        m.tool_calls = vec![serde_json::json!({
            "id": "call_0", "type": "function",
            "function": {"name": "bash", "arguments": "{}"}
        })];
        let req = request(vec![m], vec![]);
        assert!(validate_request(&req).is_ok());
    }

    #[test]
    fn tool_choice_none_drops_tools_and_adds_directive() {
        let mut req = request(vec![msg_with("user", "hi")], vec![bash_tool()]);
        req.tool_choice = Some(serde_json::json!("none"));
        let (specs, required) = resolve_tools(&req);
        assert!(specs.is_empty());
        assert!(!required);
        let (delta, _) = build_prompts(&req, None, "", None);
        assert!(delta.contains("Do not call tools in this turn; reply with normal text."));
    }

    #[test]
    fn tool_choice_required_adds_must_call_directive() {
        let mut req = request(vec![msg_with("user", "hi")], vec![bash_tool()]);
        req.tool_choice = Some(serde_json::json!("required"));
        let (_, required) = resolve_tools(&req);
        assert!(required);
        let (delta, _) = build_prompts(&req, None, "", None);
        assert!(delta.contains("You must return a tool call."));
    }

    #[test]
    fn named_tool_choice_narrows_tools() {
        let mut req = request(vec![msg_with("user", "hi")], vec![bash_tool()]);
        req.tool_choice = Some(serde_json::json!({
            "type": "function",
            "function": {"name": "bash"}
        }));
        let (specs, required) = resolve_tools(&req);
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "bash");
        assert!(required);
    }

    #[test]
    fn parallel_tool_calls_false_adds_directive() {
        let mut req = request(vec![msg_with("user", "hi")], vec![bash_tool()]);
        req.parallel_tool_calls = false;
        let (delta, _) = build_prompts(&req, None, "", None);
        assert!(delta.contains("Return at most one tool call."));
    }

    #[test]
    fn recovery_rejects_unknown_tool_and_dumps_schemas() {
        let specs = dialect::tool_specs(&[tool_with_schema()]);
        let mut recovery = Recovery::new(specs, false, true, dialect::Dialect::JsonInvoke);
        let produced = recovery
            .accept(dialect::ScanEvent::Tool(dialect::ToolCall {
                name: "nope".into(),
                arguments: "{}".into(),
            }))
            .unwrap();
        assert!(produced.is_none());
        assert!(recovery.has_pending());
        let prompt = recovery.begin_repair(1);
        assert!(prompt.contains("unknown_or_disallowed_tool"));
        assert!(prompt.contains("Valid tool names: bash."));
    }

    #[test]
    fn recovery_required_call_missing_becomes_a_failure() {
        let specs = dialect::tool_specs(&[tool_with_schema()]);
        let mut recovery = Recovery::new(specs, true, true, dialect::Dialect::JsonInvoke);
        recovery.require_call();
        assert!(recovery.has_pending());
        assert_eq!(recovery.pending[0].reason, "required_tool_call_missing");
    }

    #[test]
    fn recovery_rejects_parallel_calls_when_disabled() {
        let specs = dialect::tool_specs(&[tool_with_schema()]);
        let mut recovery = Recovery::new(specs, false, false, dialect::Dialect::JsonInvoke);
        let ok = dialect::ScanEvent::Tool(dialect::ToolCall {
            name: "bash".into(),
            arguments: "{\"command\": \"ls\"}".into(),
        });
        assert!(recovery.accept(ok.clone()).unwrap().is_some());
        let second = recovery.accept(ok).unwrap_err();
        assert_eq!(second.code, "tool_repair_failed");
    }

    #[test]
    fn history_is_role_labelled() {
        let text = flatten_transcript(&[
            msg_with("system", "be nice"),
            msg_with("user", "hi"),
            msg_with("assistant", "hello"),
        ], dialect::Dialect::JsonInvoke);
        assert_eq!(text, "System: be nice\n\nUser: hi\n\nAssistant: hello");
    }

    #[test]
    fn assistant_tool_calls_are_reserialized() {
        let mut m = msg_with("assistant", "");
        m.tool_calls = vec![serde_json::json!({
            "type": "function",
            "function": {"name": "bash", "arguments": "{\"command\":\"ls\"}"}
        })];
        assert_eq!(
            render_message(&m, dialect::Dialect::JsonInvoke).unwrap(),
            "Assistant:\n<invoke name=\"bash\">{\"command\":\"ls\"}</invoke>"
        );
    }

    #[test]
    fn reminder_is_appended_to_every_turn() {
        let tools = vec![bash_tool()];
        // Fresh thread: reminder goes after the transcript.
        let req = request(vec![msg_with("user", "hi")], tools.clone());
        let (delta, replay) = build_prompts(&req, None, "", None);
        assert!(delta.contains("User: hi"));
        assert!(delta.ends_with(&dialect::periodic_reminder()));
        assert_eq!(replay.matches(&dialect::periodic_reminder()).count(), 1);
        assert!(replay.ends_with(&dialect::periodic_reminder()));

        // Continuation: still appended, still last.
        let req = request(vec![msg_with("user", "again")], tools);
        let (delta, _) = build_prompts(&req, Some(&thread(3)), "", None);
        assert!(delta.starts_with("User: again"));
        assert!(delta.ends_with(&dialect::periodic_reminder()));
    }

    #[test]
    fn no_tools_means_no_reminder() {
        let req = request(vec![msg_with("user", "hi")], Vec::new());
        let (delta, replay) = build_prompts(&req, None, "", None);
        assert_eq!(delta, "User: hi");
        assert_eq!(replay, "User: hi");
    }

    #[test]
    fn repair_prompt_carries_only_the_failed_tools_schema() {
        // A schema dump helps a correction turn learn the argument names for the
        // tool it already named. It must NOT include tools that did not fail:
        // that grows the repair prompt past the turn it repairs, in exactly the
        // context-pressure situation where repair is most likely needed.
        let other = serde_json::json!({
            "type": "function",
            "function": {
                "name": "search",
                "description": "look things up",
                "parameters": {"type": "object", "properties": {"query": {"type": "string"}}}
            }
        });
        let specs = dialect::tool_specs(&[tool_with_schema(), other]);
        let mut recovery = Recovery::new(specs, false, true, dialect::Dialect::JsonInvoke);
        recovery.pending.push(dialect::MalformedCall::new(
            Some("bash".into()),
            "{}".into(),
            "arguments_do_not_match_schema",
        ));
        let prompt = recovery.begin_repair(1);
        assert!(prompt.contains("Schema(s) for the tool call(s) that failed:"));
        assert!(
            prompt.contains("\"command\""),
            "schema must survive the dump"
        );
        assert!(
            prompt.contains("required"),
            "required must survive the dump"
        );
        assert!(
            !prompt.contains("\"search\""),
            "unfailed tools must not be dumped"
        );
    }

    #[test]
    fn repair_prompt_sends_no_schema_for_unknown_or_missing_calls() {
        let specs = dialect::tool_specs(&[tool_with_schema()]);
        for (name, reason) in [
            (Some("nope".to_string()), "unknown_or_disallowed_tool"),
            (None, "required_tool_call_missing"),
            (None, "repair_omitted_call"),
            (Some("bash".to_string()), "unterminated_block"),
        ] {
            let mut recovery = Recovery::new(specs.clone(), false, true, dialect::Dialect::JsonInvoke);
            recovery.pending.push(dialect::MalformedCall::new(
                name.clone(),
                String::new(),
                reason,
            ));
            let prompt = recovery.begin_repair(1);
            assert!(
                !prompt.contains("Schema(s) for the tool call(s) that failed:"),
                "{reason} must not trigger a schema dump"
            );
            // The failure is still named, and the valid names are still listed.
            assert!(prompt.contains(reason), "{reason} must be reported");
            assert!(prompt.contains("Valid tool names: bash."));
        }
    }

    /// The live path: real model text -> scan -> accept -> begin_repair.
    ///
    /// The hand-built `MalformedCall` tests passed while this was broken,
    /// because they used reason strings that never occur at runtime. A body
    /// that is not JSON, or is not an object, still names a real tool, so the
    /// repair turn must carry that tool's schema.
    #[test]
    fn wire_shape_failures_send_the_named_tools_schema() {
        let bad_json = concat!("<in", "voke name=\"bash\">oops</in", "voke>");
        let not_object = concat!("<in", "voke name=\"bash\">[1,2]</in", "voke>");
        for text in [bad_json, not_object] {
            let specs = dialect::tool_specs(&[tool_with_schema()]);
            let mut recovery = Recovery::new(specs, false, true, dialect::Dialect::JsonInvoke);
            for event in dialect::scan(text) {
                recovery.accept(event).expect("malformed is not an error");
            }
            assert!(!recovery.pending.is_empty(), "{text} must reach repair");
            let prompt = recovery.begin_repair(1);
            assert!(
                prompt.contains("Schema(s) for the tool call(s) that failed:"),
                "{text} must carry a schema"
            );
            assert!(
                prompt.contains("\"command\""),
                "{text}: schema body missing"
            );
        }
    }

    #[test]
    fn repair_schemas_dedupes_repeated_failures() {
        let specs = dialect::tool_specs(&[tool_with_schema()]);
        let bad = |name: &str| {
            dialect::MalformedCall::new(
                Some(name.into()),
                "{}".into(),
                "arguments_do_not_match_schema",
            )
        };
        let schemas = dialect::repair_schemas(&[bad("bash"), bad("bash")], &specs);
        assert_eq!(schemas.len(), 1);
    }

    #[test]
    fn route_all_keeps_the_last_message_id() {
        // aiodeepseek's `ask_stream` keeps the final non-None message id; an
        // early one must not win, or the next turn branches.
        let raw = concat!(
            "data: {\"response_message_id\": 10}\n\n",
            "data: {\"response_message_id\": 11}\n\n",
        );
        assert_eq!(route_all(raw).2, Some(11));
    }

    #[test]
    fn error_payload_type_follows_status() {
        // SDKs branch on `error.type`, so it must track the status, not the code.
        for (status, expected) in [
            (axum::http::StatusCode::BAD_REQUEST, "invalid_request_error"),
            (axum::http::StatusCode::UNAUTHORIZED, "invalid_request_error"),
            (axum::http::StatusCode::NOT_FOUND, "invalid_request_error"),
            (axum::http::StatusCode::TOO_MANY_REQUESTS, "rate_limit_error"),
            (axum::http::StatusCode::BAD_GATEWAY, "server_error"),
            (axum::http::StatusCode::SERVICE_UNAVAILABLE, "server_error"),
        ] {
            let e = ProxyError::new("x", status, "some_code");
            let payload = e.payload();
            assert_eq!(payload["error"]["type"], expected, "status {status}");
            assert_eq!(payload["error"]["code"], "some_code");
        }
    }

    /// The build stamp is a diagnostic: a wrong date is worse than no date,
    /// because it would send someone looking at the wrong binary.
    #[test]
    fn build_id_civil_dates_are_correct() {
        // Known-good pairs against the Unix epoch, including a leap day and the
        // new year boundary.
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(1), (1970, 1, 2));
        assert_eq!(civil_from_days(59), (1970, 3, 1));
        // 2000-02-29: a leap day in a leap century.
        assert_eq!(civil_from_days(11016), (2000, 2, 29));
        assert_eq!(civil_from_days(364), (1970, 12, 31));
        assert_eq!(civil_from_days(365), (1971, 1, 1));
    }

    #[test]
    fn reminder_text_names_the_failure_modes() {
        let text = dialect::periodic_reminder();
        println!("-----BEGIN-----\n{text}\n-----END-----");
        assert!(text.contains("dropped and never run"));
        assert!(text.contains("Emit the closing tag exactly once per tool call."));
        assert!(text.contains(dialect::TEMPLATE));
    }
}
