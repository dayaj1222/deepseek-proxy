//! Mock DeepSeek upstream plus a helper that boots the real proxy binary.
//!
//! The proxy talks to whatever `DEEPSEEK_BASE_URL` points at, so the whole
//! stack — axum surface, turn preparation, pool, PoW, SSE pump — runs for real
//! against a local mock. No DeepSeek account, no network.

#![allow(dead_code)]

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
    Router,
};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A recorded reply: `17*23` answered as `17*23 = 391`.
pub const PLAIN_SSE: &str = include_str!("../../../../corpus/plain.sse");

/// What the mock should do for the next completion request.
#[derive(Clone, Debug)]
pub enum Step {
    /// Replay a recorded SSE body.
    Sse(String),
    /// 200 with an SSE `event: hint` error, as the backend does when throttling.
    HintError(&'static str),
    /// 401, so the client classifies it as a dead login.
    Unauthorized,
    /// 200 with a raw JSON body and no SSE frames, as the backend answers when
    /// it refuses the request outright (`{"code":26,...}` for a stale parent).
    NonSse(String),
    /// 500 with no body: a failure that is neither a throttle nor a dead login,
    /// which is what counts against an account's circuit breaker.
    ServerError,
}

/// The scenario the mock plays, one step per completion request.
#[derive(Clone)]
pub struct MockScript {
    steps: Arc<Mutex<VecDeque<Step>>>,
    completions: Arc<Mutex<u32>>,
    /// Accounts whose login must fail (failover tests).
    dead_logins: Arc<Mutex<Vec<String>>>,
    /// `parent_message_id` of every completion request, in order. This is how a
    /// test proves the proxy resent on a *previous* cursor after the backend
    /// rejected the newest one.
    parents: Arc<Mutex<Vec<Option<i64>>>>,
    /// The `prompt` of every completion request, in order, so a test can tell a
    /// continuation (delta only) from a replay (whole transcript).
    prompts: Arc<Mutex<Vec<String>>>,
}

impl MockScript {
    pub fn new(steps: Vec<Step>) -> Self {
        Self {
            steps: Arc::new(Mutex::new(steps.into())),
            completions: Arc::new(Mutex::new(0)),
            dead_logins: Arc::new(Mutex::new(Vec::new())),
            parents: Arc::new(Mutex::new(Vec::new())),
            prompts: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn with_dead_logins(steps: Vec<Step>, dead: &[&str]) -> Self {
        let script = Self::new(steps);
        *script.dead_logins.lock().unwrap() = dead.iter().map(|s| s.to_string()).collect();
        script
    }

    /// How many completion requests the proxy actually made.
    pub fn completions(&self) -> u32 {
        *self.completions.lock().unwrap()
    }

    /// `parent_message_id` sent with each completion request, in order.
    pub fn parents(&self) -> Vec<Option<i64>> {
        self.parents.lock().unwrap().clone()
    }

    /// `prompt` sent with each completion request, in order.
    pub fn prompts(&self) -> Vec<String> {
        self.prompts.lock().unwrap().clone()
    }
}

/// Start the mock upstream; returns its base URL and the script handle.
pub async fn start_mock(script: MockScript) -> (String, MockScript) {
    let app = Router::new()
        .route("/api/v0/users/login", post(login))
        .route("/api/v0/chat_session/create", post(create_session))
        .route("/api/v0/chat/create_pow_challenge", post(pow_challenge))
        .route("/api/v0/chat/completion", post(completion))
        .with_state(script.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), script)
}

async fn login(State(script): State<MockScript>, body: String) -> Response {
    let email = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|v| v.get("email").and_then(|e| e.as_str()).map(str::to_string))
        .unwrap_or_default();
    if script
        .dead_logins
        .lock()
        .unwrap()
        .iter()
        .any(|d| d == &email)
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    axum::Json(json!({
        "code": 0,
        "data": {"biz_data": {"user": {"token": format!("token-{email}")}}}
    }))
    .into_response()
}

async fn create_session() -> Response {
    axum::Json(json!({
        "code": 0,
        "data": {"biz_data": {"chat_session": {"id": "mock-session"}}}
    }))
    .into_response()
}

async fn pow_challenge() -> Response {
    // A trivially solvable challenge: difficulty 16 means the solver scans at
    // most 16 nonces, so the real PoW code runs but stays fast. The answer is
    // nonce 0, because the challenge is the hash of base + "0".
    let salt = "mock-salt";
    let expire_at = "1799999999999";
    let base = format!("{salt}_{expire_at}_");
    let challenge = pow::deepseek_hash(&base, 0);
    let hex: String = challenge.iter().map(|b| format!("{b:02x}")).collect();
    axum::Json(json!({
        "code": 0,
        "data": {"biz_data": {
            "challenge": {
                "algorithm": "DeepSeekHashV1",
                "challenge": hex,
                "salt": salt,
                "expire_at": expire_at,
                "difficulty": 16,
                "signature": "mock-signature",
            }
        }}
    }))
    .into_response()
}

async fn completion(State(script): State<MockScript>, body: String) -> Response {
    *script.completions.lock().unwrap() += 1;
    let parsed = serde_json::from_str::<Value>(&body).ok();
    script.parents.lock().unwrap().push(
        parsed
            .as_ref()
            .and_then(|v| v.get("parent_message_id").and_then(Value::as_i64)),
    );
    script.prompts.lock().unwrap().push(
        parsed
            .as_ref()
            .and_then(|v| v.get("prompt").and_then(Value::as_str))
            .unwrap_or_default()
            .to_string(),
    );
    let step = script.steps.lock().unwrap().pop_front();
    let sse = [(axum::http::header::CONTENT_TYPE, "text/event-stream")];
    match step {
        Some(Step::Sse(body)) => (StatusCode::OK, sse, body).into_response(),
        Some(Step::NonSse(body)) => {
            let json = [(axum::http::header::CONTENT_TYPE, "application/json")];
            (StatusCode::OK, json, body).into_response()
        }
        Some(Step::ServerError) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        Some(Step::HintError(message)) => {
            let body = format!(
                "event: hint\ndata: {}\n\n",
                json!({"type": "error", "content": message})
            );
            (StatusCode::OK, sse, body).into_response()
        }
        Some(Step::Unauthorized) => StatusCode::UNAUTHORIZED.into_response(),
        // No scripted step left: replay the plain fixture.
        None => (StatusCode::OK, sse, PLAIN_SSE).into_response(),
    }
}

/// A proxy process pointed at the mock, with its own runtime dir and DB.
pub struct App {
    pub base: String,
    child: std::process::Child,
    dir: std::path::PathBuf,
    config: std::path::PathBuf,
}

impl App {
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Send SIGHUP, which the server treats as "re-read the config".
    pub fn sighup(&self) {
        let rc = unsafe { libc::kill(self.pid() as i32, libc::SIGHUP) };
        assert_eq!(rc, 0, "kill(SIGHUP) failed");
    }

    /// Rewrite the running config, the way an operator would before a SIGHUP.
    pub fn write_config(&self, body: &str) {
        std::fs::write(&self.config, body).expect("write config.toml");
    }

    /// Paths the proxy was started with, so a test can run the real CLI against
    /// the same state directory instead of guessing.
    pub fn db_path(&self) -> std::path::PathBuf {
        self.dir.join("test.db")
    }

    pub fn runtime_dir(&self) -> &std::path::Path {
        &self.dir
    }

    pub fn config_path(&self) -> &std::path::Path {
        &self.config
    }
}

impl Drop for App {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

/// Boot the real binary against `upstream`, with `accounts` email/password
/// pairs configured through the environment.
pub async fn spawn_app(upstream: &str, accounts: &[(&str, &str)]) -> App {
    spawn_app_with_keys(upstream, accounts, &[]).await
}

/// Poll `/stats` until `pred` accepts it, or fail after ~5s.
///
/// A reload is asynchronous by design — the signal is delivered, then the
/// config is re-read — so tests must wait for the effect rather than sleep a
/// guessed amount and hope.
pub async fn stats_until(
    app: &App,
    what: &str,
    mut pred: impl FnMut(&serde_json::Value) -> bool,
) -> serde_json::Value {
    let mut last = serde_json::Value::Null;
    for _ in 0..100 {
        let resp = reqwest::get(format!("{}/stats", app.base))
            .await
            .expect("GET /stats");
        last = resp.json().await.expect("stats json");
        if pred(&last) {
            return last;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}; last stats: {last}");
}

/// `spawn_app` with extra environment overrides, for config that is read once
/// at boot (the breaker thresholds, for instance).
pub async fn spawn_app_with_env(
    upstream: &str,
    accounts: &[(&str, &str)],
    envs: &[(&str, &str)],
) -> App {
    spawn_with(upstream, accounts, &[], envs).await
}

/// Same as `spawn_app`, but registers `keys` (name, secret) first, so the proxy
/// requires a bearer token. Registering goes through the real `keys add` CLI,
/// which is the only public way to seed the key table.
pub async fn spawn_app_with_keys(
    upstream: &str,
    accounts: &[(&str, &str)],
    keys: &[(&str, &str)],
) -> App {
    spawn_with(upstream, accounts, keys, &[]).await
}

async fn spawn_with(
    upstream: &str,
    accounts: &[(&str, &str)],
    keys: &[(&str, &str)],
    envs: &[(&str, &str)],
) -> App {
    let port = free_port();
    let dir =
        std::env::temp_dir().join(format!("deepseek-rs-test-{}-{}", std::process::id(), port));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // Every spawned proxy gets its own config file, and `CONFIG_PATH` points at
    // it. Without this a test would inherit the repo's real `config.toml`, and
    // the hot-reload tests would have nothing safe to rewrite.
    let config_path = dir.join("config.toml");
    std::fs::write(&config_path, "").expect("seed config.toml");

    for (name, secret) in keys {
        let status = std::process::Command::new(env!("CARGO_BIN_EXE_deepseek-rs"))
            .arg("keys")
            .arg("add")
            .arg(name)
            .arg(secret)
            .env("DB_PATH", dir.join("test.db"))
            .env("DEEPSEEK_RUNTIME_DIR", &dir)
            .env("RUST_LOG", "warn")
            .status()
            .expect("run keys add");
        assert!(status.success(), "keys add {name} failed");
    }

    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_deepseek-rs"));
    cmd.arg("serve")
        .env("DEEPSEEK_BASE_URL", upstream)
        .env("PROXY_HOST", "127.0.0.1")
        .env("PROXY_PORT", port.to_string())
        .env("DB_PATH", dir.join("test.db"))
        .env("CONFIG_PATH", &config_path)
        .env("DEEPSEEK_RUNTIME_DIR", &dir)
        // Never inherit the developer's real accounts from `../deepseek/.env`:
        // a test's account list must be exactly what the test declared.
        .env("DEEPSEEK_NO_DOTENV", "1")
        .env("RUST_LOG", "warn")
        // Keep the scenario fast: no sleep between requests, no retry backoff.
        .env("REQUEST_DELAY", "0")
        .env("RATE_LIMIT_BACKOFF_S", "0")
        .env("TOOL_REPAIR_MAX_RETRIES", "1");
    for (key, value) in envs {
        cmd.env(key, value);
    }
    for (i, (email, password)) in accounts.iter().enumerate() {
        cmd.env(format!("DEEPSEEK_EMAIL_{i}"), email)
            .env(format!("DEEPSEEK_PASSWORD_{i}"), password);
    }
    let child = cmd.spawn().expect("spawn proxy binary");

    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::new();
    for _ in 0..200 {
        if client
            .get(format!("{base}/healthz"))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
        {
            return App {
                base,
                child,
                dir,
                config: config_path,
            };
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("proxy did not become healthy at {base}");
}

/// Post one chat completion and return the raw response.
pub async fn complete(app: &App, body: Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", app.base))
        .json(&body)
        .send()
        .await
        .expect("proxy request")
}

/// Post one chat completion with a bearer token, so auth can be exercised.
pub async fn complete_with_key(app: &App, body: Value, key: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", app.base))
        .bearer_auth(key)
        .json(&body)
        .send()
        .await
        .expect("proxy request")
}

/// Build an SSE reply body the way DeepSeek streams one: a `response` object
/// carrying the message ids, then text fragments appended to it.
///
/// Keeping this here means tests never have to embed escaped JSON, which is
/// where hand-written SSE fixtures usually go wrong.
pub fn sse_reply(message_id: i64, parent_id: i64, fragments: &[&str]) -> String {
    let mut out = String::new();
    // The `ready` event is where the proxy learns the cursor: it reads the
    // top-level `response_message_id`, not `v.response.message_id`. Without this
    // line a scripted reply commits no parent id at all.
    let ready = json!({
        "request_message_id": parent_id,
        "response_message_id": message_id,
        "model_type": "default"
    });
    out.push_str(&format!("event: ready\ndata: {ready}\n\n"));
    let response = json!({
        "v": {"response": {
            "message_id": message_id,
            "parent_id": parent_id,
            "role": "ASSISTANT",
            "fragments": [{"id": message_id, "type": "RESPONSE", "content": ""}]
        }}
    });
    out.push_str(&format!("data: {response}\n\n"));
    for fragment in fragments {
        let line = json!({"v": fragment});
        out.push_str(&format!("data: {line}\n\n"));
    }
    out
}

/// A minimal user turn.
pub fn hello_turn() -> Value {
    json!({
        "model": "deepseek-chat",
        "messages": [{"role": "user", "content": "17*23"}]
    })
}
