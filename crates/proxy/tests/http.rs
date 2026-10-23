//! HTTP-level tests over the real router against a mock upstream.
//!
//! Every test boots the actual `deepseek-rs` binary, points it at a local mock
//! DeepSeek, and drives `/v1/chat/completions`. Nothing reaches the network and
//! no account is required, which is what T-19 exists for.

mod support;

use serde_json::json;
use support::{
    complete, complete_with_key, hello_turn, spawn_app, spawn_app_with_env, spawn_app_with_keys,
    sse_reply, start_mock, stats_until, MockScript, Step, PLAIN_SSE,
};

/// Concatenate every delta content string in an SSE body.
/// `hello_turn()` with `stream: true`.
///
/// Most tests want the SSE transport because it is what they assert on; the
/// non-streaming path has its own tests. Defaulting the helper keeps every
/// existing body assertion meaningful.
fn streamed(mut body: serde_json::Value) -> serde_json::Value {
    body["stream"] = json!(true);
    body
}

fn body_text(body: &str) -> String {
    let mut out = String::new();
    for line in body.lines() {
        let Some(payload) = line.trim().strip_prefix("data:") else {
            continue;
        };
        let payload = payload.trim();
        if payload.is_empty() || payload == "[DONE]" {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(payload) else {
            continue;
        };
        if let Some(text) = value
            .pointer("/choices/0/delta/content")
            .and_then(|v| v.as_str())
        {
            out.push_str(text);
        }
    }
    out
}

#[tokio::test]
async fn health_and_models_are_served() {
    let (upstream, _script) = start_mock(MockScript::new(vec![])).await;
    let app = spawn_app(&upstream, &[("a@example.com", "pw")]).await;

    let client = reqwest::Client::new();
    let health = client
        .get(format!("{}/healthz", app.base))
        .send()
        .await
        .unwrap();
    assert!(health.status().is_success());

    let models = client
        .get(format!("{}/v1/models", app.base))
        .send()
        .await
        .unwrap();
    assert!(models.status().is_success());
}

#[tokio::test]
async fn streaming_completion_replays_the_upstream_answer() {
    let (upstream, script) = start_mock(MockScript::new(vec![])).await;
    let app = spawn_app(&upstream, &[("a@example.com", "pw")]).await;

    let mut body = hello_turn();
    body["stream"] = json!(true);
    let resp = complete(&app, body).await;
    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();

    assert!(text.contains("[DONE]"), "stream must terminate: {text}");
    assert!(
        body_text(&text).contains("391"),
        "expected the fixture answer, got: {text}"
    );
    assert_eq!(script.completions(), 1);
}

#[tokio::test]
async fn a_non_streaming_request_still_gets_a_complete_answer() {
    // `stream: false` must be a single `chat.completion` JSON object. It used
    // to be an SSE body, which no OpenAI SDK can parse — the client got a 200
    // and then failed to deserialize it.
    let (upstream, _script) = start_mock(MockScript::new(vec![])).await;
    let app = spawn_app(&upstream, &[("a@example.com", "pw")]).await;

    let resp = complete(&app, hello_turn()).await;
    assert_eq!(resp.status(), 200);
    assert!(
        header(&resp, "content-type").starts_with("application/json"),
        "non-streaming responses must be JSON, got {}",
        header(&resp, "content-type")
    );

    let body: serde_json::Value = resp.json().await.expect("a JSON body, not SSE");
    assert_eq!(body["object"], json!("chat.completion"));
    assert!(
        body.pointer("/choices/0/message/content")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .contains("391"),
        "the answer must be in choices[0].message.content: {body}"
    );
    assert_eq!(body["choices"][0]["finish_reason"], json!("stop"));
    // The collector asks the pump for usage, so a non-streaming client still
    // gets exact token counts rather than none.
    assert!(
        body.pointer("/usage/completion_tokens")
            .and_then(|v| v.as_u64())
            .is_some_and(|n| n > 0),
        "usage must be reported: {body}"
    );
}

/// A non-streaming request that produces a tool call must return it in
/// `message.tool_calls`, the shape every SDK expects, not as SSE deltas.
#[tokio::test]
async fn a_non_streaming_tool_call_is_in_the_message() {
    let good = format!("{TOOL_OPEN}{{\"command\": \"ls\"}}{TOOL_CLOSE}");
    let script = MockScript::new(vec![Step::Sse(sse_reply(3, 2, &[good.as_str()]))]);
    let (upstream, _script) = start_mock(script).await;
    let app = spawn_app(&upstream, &[("a@example.com", "pw")]).await;

    let mut body = hello_turn();
    body["tools"] = json!([{
        "type": "function",
        "function": {
            "name": "bash",
            "parameters": {
                "type": "object",
                "properties": {"command": {"type": "string"}},
                "required": ["command"]
            }
        }
    }]);
    let resp = complete(&app, body).await;
    assert_eq!(resp.status(), 200);
    let value: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(value["choices"][0]["finish_reason"], json!("tool_calls"));
    let call = &value["choices"][0]["message"]["tool_calls"][0];
    assert_eq!(call["type"], json!("function"));
    assert_eq!(call["function"]["name"], json!("bash"));
    assert!(
        call["function"]["arguments"]
            .as_str()
            .unwrap_or_default()
            .contains("ls"),
        "arguments must be the JSON string: {value}"
    );
}

#[tokio::test]
async fn a_throttled_turn_is_retried_then_succeeds() {
    let script = MockScript::new(vec![
        Step::HintError("Messages too frequent. Try again later."),
        Step::Sse(PLAIN_SSE.to_string()),
    ]);
    let (upstream, script) = start_mock(script).await;
    let app = spawn_app(&upstream, &[("a@example.com", "pw")]).await;

    let mut body = hello_turn();
    body["stream"] = json!(true);
    let resp = complete(&app, body).await;
    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();
    assert!(body_text(&text).contains("391"), "{text}");
    assert_eq!(
        script.completions(),
        2,
        "one throttle plus one successful retry"
    );
}

#[tokio::test]
async fn a_dead_login_fails_over_to_a_healthy_account() {
    // The dead account cannot log in at all, so boot must still succeed and the
    // turn must land on the healthy one.
    let (upstream, script) =
        start_mock(MockScript::with_dead_logins(vec![], &["dead@example.com"])).await;
    let app = spawn_app(
        &upstream,
        &[("live@example.com", "pw"), ("dead@example.com", "pw")],
    )
    .await;

    let resp = complete(&app, streamed(hello_turn())).await;
    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();
    assert!(body_text(&text).contains("391"), "{text}");
    assert_eq!(script.completions(), 1);
}

#[tokio::test]
async fn an_unknown_model_is_rejected_before_any_upstream_call() {
    let (upstream, script) = start_mock(MockScript::new(vec![])).await;
    let app = spawn_app(&upstream, &[("a@example.com", "pw")]).await;

    let mut body = hello_turn();
    body["model"] = json!("gpt-4");
    let resp = complete(&app, body).await;
    assert_eq!(resp.status(), 400);
    let value: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        value.pointer("/error/code").and_then(|v| v.as_str()),
        Some("model_not_found")
    );
    assert_eq!(script.completions(), 0);
}
#[tokio::test]
async fn malformed_tool_call_is_repaired_and_delivered() {
    let bad = format!("{TOOL_OPEN}{{oops}}{TOOL_CLOSE}");
    let good = format!("{TOOL_OPEN}{{\"command\": \"ls\"}}{TOOL_CLOSE}");
    let malformed = sse_reply(3, 2, &[bad.as_str()]);
    let repaired = sse_reply(4, 3, &[good.as_str()]);
    let script = MockScript::new(vec![Step::Sse(malformed), Step::Sse(repaired)]);
    let (upstream, script) = start_mock(script).await;
    let app = spawn_app(&upstream, &[("a@example.com", "pw")]).await;
    let body = json!({
        "model": "deepseek-chat",
        "stream": true,
        "messages": [{"role": "user", "content": "list files"}],
        "tools": [{"type": "function", "function": {
            "name": "bash", "description": "run a command",
            "parameters": {"type": "object", "properties": {"command": {"type": "string"}}, "required": ["command"]}
        }}]
    });
    let resp = complete(&app, body).await;
    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();
    assert!(text.contains("tool_calls"), "repaired call missing: {text}");
    assert_eq!(script.completions(), 2, "one repair turn expected");
}

/// The poisoned-cursor regression: DeepSeek answers `{"code":26}` when the
/// saved parent no longer exists. The proxy must step back one commit and resend
/// on the previous cursor rather than abandoning the session and replaying the
/// whole transcript (which is what tripped the upstream throttle in production).
#[tokio::test]
async fn a_rejected_parent_id_rewinds_to_the_previous_cursor() {
    let stale = json!({
        "code": 0, "msg": "",
        "data": {"biz_code": 26, "biz_msg": "invalid message id", "biz_data": null}
    })
    .to_string();
    let script = MockScript::new(vec![
        Step::Sse(sse_reply(2, 1, &["391"])),
        Step::Sse(sse_reply(3, 2, &["782"])),
        Step::NonSse(stale),
        Step::Sse(sse_reply(4, 2, &["1173"])),
    ]);
    let (upstream, script) = start_mock(script).await;
    let app = spawn_app(&upstream, &[("a@example.com", "pw")]).await;

    // `thread_id` pins every request to one conversation, so the transcript
    // shape does not change the thread key between turns.
    let turn = |messages: serde_json::Value| {
        json!({
            "model": "deepseek-chat",
            "stream": true,
            "thread_id": "rewind-test",
            "messages": messages
        })
    };

    // Two successful turns, so the thread has a cursor to walk back to. Each
    // request must END on the user message: the continuation prompt is built
    // from the messages after the last assistant one, so a transcript that ends
    // on an assistant reply has nothing to send.
    let resp = complete(&app, turn(json!([{"role": "user", "content": "17*23"}]))).await;
    assert_eq!(resp.status(), 200);
    resp.text().await.unwrap();

    let resp = complete(
        &app,
        turn(json!([
            {"role": "user", "content": "17*23"},
            {"role": "assistant", "content": "391"},
            {"role": "user", "content": "34*23"}
        ])),
    )
    .await;
    assert_eq!(resp.status(), 200);
    resp.text().await.unwrap();

    // Third turn: parent id 3 no longer exists upstream, so the reply is
    // rejected and the retry must go out on cursor 2.
    let resp = complete(
        &app,
        turn(json!([
            {"role": "user", "content": "17*23"},
            {"role": "assistant", "content": "391"},
            {"role": "user", "content": "34*23"},
            {"role": "assistant", "content": "782"},
            {"role": "user", "content": "51*23"}
        ])),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();
    assert!(
        body_text(&text).contains("1173"),
        "rewound retry should deliver the answer: {text}"
    );

    assert_eq!(
        script.parents(),
        vec![None, Some(2), Some(3), Some(2)],
        "the retry must resend on the previous cursor, not the rejected one"
    );
    assert_eq!(
        script.completions(),
        4,
        "two turns, one rejection, one rewound retry"
    );
}

#[tokio::test]
async fn a_missing_key_is_a_401_in_the_openai_envelope() {
    let (upstream, script) = start_mock(MockScript::new(vec![])).await;
    let app = spawn_app_with_keys(
        &upstream,
        &[("a@example.com", "pw")],
        &[("main", "sk-test-secret")],
    )
    .await;

    let resp = complete(&app, hello_turn()).await;
    assert_eq!(resp.status(), 401);
    assert_eq!(
        resp.headers()
            .get("www-authenticate")
            .and_then(|v| v.to_str().ok()),
        Some("Bearer")
    );
    let value: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        value.pointer("/error/code").and_then(|v| v.as_str()),
        Some("missing_api_key")
    );
    assert_eq!(
        value.pointer("/error/type").and_then(|v| v.as_str()),
        Some("invalid_request_error")
    );
    assert_eq!(script.completions(), 0);
}

#[tokio::test]
async fn a_wrong_key_is_a_401_without_echoing_it() {
    let (upstream, script) = start_mock(MockScript::new(vec![])).await;
    let app = spawn_app_with_keys(
        &upstream,
        &[("a@example.com", "pw")],
        &[("main", "sk-test-secret")],
    )
    .await;

    let resp = complete_with_key(&app, hello_turn(), "sk-wrong").await;
    assert_eq!(resp.status(), 401);
    let text = resp.text().await.unwrap();
    assert!(!text.contains("sk-wrong"), "key echoed back: {text}");
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        value.pointer("/error/code").and_then(|v| v.as_str()),
        Some("invalid_api_key")
    );
    assert_eq!(script.completions(), 0);
}

#[tokio::test]
async fn an_unknown_path_is_json_not_an_empty_404() {
    let (upstream, _script) = start_mock(MockScript::new(vec![])).await;
    let app = spawn_app(&upstream, &[("a@example.com", "pw")]).await;

    let resp = reqwest::Client::new()
        .get(format!("{}/v1/nope", app.base))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    let value: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        value.pointer("/error/code").and_then(|v| v.as_str()),
        Some("not_found")
    );
}

#[tokio::test]
async fn a_wrong_method_on_a_known_path_is_json_405() {
    let (upstream, _script) = start_mock(MockScript::new(vec![])).await;
    let app = spawn_app(&upstream, &[("a@example.com", "pw")]).await;

    let resp = reqwest::Client::new()
        .get(format!("{}/v1/chat/completions", app.base))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 405);
    let value: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        value.pointer("/error/code").and_then(|v| v.as_str()),
        Some("method_not_allowed")
    );
}

#[tokio::test]
async fn every_response_carries_an_x_request_id() {
    // Success, stream, 401, 404 and 405 all go through the same middleware; a
    // client must be able to quote one id for any of them when reporting a bug.
    let (upstream, _script) = start_mock(MockScript::new(vec![])).await;
    let app = spawn_app_with_keys(
        &upstream,
        &[("a@example.com", "pw")],
        &[("main", "sk-test-secret")],
    )
    .await;
    let client = reqwest::Client::new();

    let ok = complete_with_key(&app, hello_turn(), "sk-test-secret").await;
    assert_eq!(ok.status(), 200);
    let ok_id = header(&ok, "x-request-id");

    let mut stream_body = hello_turn();
    stream_body["stream"] = json!(true);
    let streamed = complete_with_key(&app, stream_body, "sk-test-secret").await;
    assert_eq!(streamed.status(), 200);
    assert!(!header(&streamed, "x-request-id").is_empty());

    let unauthorized = complete(&app, hello_turn()).await;
    assert_eq!(unauthorized.status(), 401);
    assert!(!header(&unauthorized, "x-request-id").is_empty());

    let missing = client
        .get(format!("{}/v1/nope", app.base))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 404);
    assert!(!header(&missing, "x-request-id").is_empty());

    let wrong_method = client
        .get(format!("{}/v1/chat/completions", app.base))
        .send()
        .await
        .unwrap();
    assert_eq!(wrong_method.status(), 405);
    assert!(!header(&wrong_method, "x-request-id").is_empty());

    // Distinct requests get distinct ids, or the id is worthless for tracing.
    assert_ne!(ok_id, header(&streamed, "x-request-id"));
}

#[tokio::test]
async fn an_inbound_request_id_is_echoed_back() {
    let (upstream, _script) = start_mock(MockScript::new(vec![])).await;
    let app = spawn_app(&upstream, &[("a@example.com", "pw")]).await;

    let resp = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", app.base))
        .header("x-request-id", "trace-abc-123")
        .json(&hello_turn())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(header(&resp, "x-request-id"), "trace-abc-123");
}

fn header(resp: &reqwest::Response, name: &str) -> String {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string()
}

/// After `account_fail_threshold` failures the account's breaker opens, the
/// failing thread moves to a healthy account, and `/stats` says so. Without
/// this, a flapping account is retried on every turn forever.
#[tokio::test]
async fn a_failing_account_opens_its_breaker_and_the_thread_moves_on() {
    let script = MockScript::new(vec![
        Step::ServerError,
        Step::Sse(sse_reply(2, 1, &["391"])),
    ]);
    let (upstream, script) = start_mock(script).await;
    let app = spawn_app_with_env(
        &upstream,
        &[("a@example.com", "pw"), ("b@example.com", "pw")],
        // One failure is enough to open it, so the test does not need three
        // upstream calls to reach the interesting state.
        &[("ACCOUNT_FAIL_THRESHOLD", "1")],
    )
    .await;

    // First turn hits the 500. It is neither a throttle nor a dead login, so it
    // counts against the account and is reported to the client.
    let resp = complete(&app, hello_turn()).await;
    assert!(!resp.status().is_success(), "the 500 must reach the client");

    // Second turn: the thread must not go back to the broken account.
    let resp = complete(&app, streamed(hello_turn())).await;
    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();
    assert!(body_text(&text).contains("391"), "{text}");
    assert_eq!(script.completions(), 2);

    let stats: serde_json::Value = reqwest::get(format!("{}/stats", app.base))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let accounts = stats["accounts"].as_array().expect("accounts array");
    assert_eq!(accounts.len(), 2);
    let open = accounts
        .iter()
        .filter(|a| a["breaker"] == json!("open"))
        .count();
    assert_eq!(open, 1, "exactly one breaker should be open: {stats}");
    assert!(
        accounts
            .iter()
            .any(|a| a["breaker"] == json!("closed") && a["healthy"] == json!(true)),
        "the other account must stay usable: {stats}"
    );
}

/// A browser preflight must succeed and carry CORS headers, and a real
/// response must expose `x-request-id` so a UI can report it.
#[tokio::test]
async fn cors_preflight_succeeds_and_responses_are_readable_by_browsers() {
    let (upstream, _script) = start_mock(MockScript::new(vec![])).await;
    let app = spawn_app(&upstream, &[("a@example.com", "pw")]).await;
    let client = reqwest::Client::new();

    let preflight = client
        .request(reqwest::Method::OPTIONS, format!("{}/v1/chat/completions", app.base))
        .header("origin", "http://localhost:3000")
        .header("access-control-request-method", "POST")
        .header("access-control-request-headers", "authorization,content-type")
        .send()
        .await
        .unwrap();
    assert!(preflight.status().is_success(), "preflight must not 405");
    assert_eq!(header(&preflight, "access-control-allow-origin"), "*");
    assert!(
        header(&preflight, "access-control-allow-headers").contains("authorization"),
        "the preflight must allow the bearer header"
    );

    let resp = complete(&app, hello_turn()).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(header(&resp, "access-control-allow-origin"), "*");
    assert!(
        header(&resp, "access-control-expose-headers").contains("x-request-id"),
        "a browser client must be able to read the request id"
    );
}

/// Endpoints this proxy does not implement answer 501 in the OpenAI envelope
/// rather than 404, so a capability probe learns the difference.
#[tokio::test]
async fn unsupported_endpoints_answer_501_not_404() {
    let (upstream, _script) = start_mock(MockScript::new(vec![])).await;
    let app = spawn_app(&upstream, &[("a@example.com", "pw")]).await;

    for path in ["/v1/embeddings", "/v1/completions", "/v1/audio/speech"] {
        let resp = reqwest::Client::new()
            .post(format!("{}{path}", app.base))
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 501, "{path} must be 501");
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["error"]["code"], json!("not_supported"), "{path}: {body}");
        assert_eq!(body["error"]["type"], json!("server_error"));
    }
}

/// A SIGHUP re-reads `config.toml` and installs it, without restarting and
/// without dropping a turn. `/stats.config.generation` is the proof it took.
#[tokio::test]
async fn a_sighup_reloads_the_config() {
    let (upstream, _script) = start_mock(MockScript::new(vec![])).await;
    let app = spawn_app(&upstream, &[("a@example.com", "pw")]).await;

    let before = stats_until(&app, "the boot config", |s| {
        s["config"]["generation"].as_u64() == Some(1)
    })
    .await;
    assert_eq!(before["config"]["generation"], json!(1));
    assert!(before["config"]["last_error"].is_null());

    // A live key, changed the way an operator would.
    app.write_config("[limits]\nrequest_timeout_secs = 42.0\n");
    app.sighup();

    let after = stats_until(&app, "generation 2", |s| {
        s["config"]["generation"].as_u64() == Some(2)
    })
    .await;
    assert!(after["config"]["last_error"].is_null(), "{after}");

    // The proxy still serves after the reload: a config swap must not disturb
    // the listener or the pool.
    let resp = complete(&app, streamed(hello_turn())).await;
    assert_eq!(resp.status(), 200);
}

/// A config that does not parse must leave the running one alone: refusing a
/// reload is correct, bricking a serving process is not.
#[tokio::test]
async fn an_invalid_config_keeps_the_running_one_and_reports_the_error() {
    let (upstream, _script) = start_mock(MockScript::new(vec![])).await;
    let app = spawn_app(&upstream, &[("a@example.com", "pw")]).await;

    stats_until(&app, "the boot config", |s| {
        s["config"]["generation"].as_u64() == Some(1)
    })
    .await;

    app.write_config("this is not toml = = =\n");
    app.sighup();

    let after = stats_until(&app, "a recorded reload error", |s| {
        !s["config"]["last_error"].is_null()
    })
    .await;
    assert_eq!(
        after["config"]["generation"],
        json!(1),
        "a failed reload must not bump the generation: {after}"
    );

    // Still serving on the old config.
    let resp = complete(&app, streamed(hello_turn())).await;
    assert_eq!(resp.status(), 200);
}

/// A SIGHUP that adds an account must make it routable without a restart, and
/// a turn in flight must not be disturbed by the pool swap.
#[tokio::test]
async fn a_reload_adds_an_account_without_restarting() {
    let (upstream, _script) = start_mock(MockScript::new(vec![])).await;
    let app = spawn_app(&upstream, &[("a@example.com", "pw")]).await;

    let before = stats_until(&app, "one account", |s| {
        s["accounts"].as_array().is_some_and(|a| a.len() == 1)
    })
    .await;
    assert_eq!(before["accounts"].as_array().unwrap().len(), 1);

    // Adding an account goes through the real CLI, which writes the row the
    // reload reads. Pointed at this app's own DB and runtime dir, so the two
    // processes share state the way they would on a real machine.
    let status = std::process::Command::new(env!("CARGO_BIN_EXE_deepseek-rs"))
        .args(["accounts", "add", "b@example.com", "pw"])
        .env("DB_PATH", app.db_path())
        .env("DEEPSEEK_NO_DOTENV", "1")
        .env("DEEPSEEK_RUNTIME_DIR", app.runtime_dir())
        .env("CONFIG_PATH", app.config_path())
        .status()
        .expect("run accounts add");
    assert!(status.success(), "accounts add failed");

    app.sighup();

    let after = stats_until(&app, "two accounts", |s| {
        s["accounts"].as_array().is_some_and(|a| a.len() == 2)
    })
    .await;
    let emails: Vec<&str> = after["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|a| a["email"].as_str())
        .collect();
    assert!(emails.contains(&"a@example.com"), "{emails:?}");
    assert!(emails.contains(&"b@example.com"), "{emails:?}");

    // And the proxy still serves on the rebuilt pool.
    let resp = complete(&app, streamed(hello_turn())).await;
    assert_eq!(resp.status(), 200);
}

/// `deepseek-rs reload` with no running server must fail loudly, not lie.
#[test]
fn reload_returns_1_without_a_pidfile() {
    let dir = std::env::temp_dir().join(format!(
        "deepseek-rs-reload-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_deepseek-rs"))
        .arg("reload")
        .env("DEEPSEEK_RUNTIME_DIR", &dir)
        .env("DEEPSEEK_NO_DOTENV", "1")
        .output()
        .expect("run reload");

    assert_eq!(out.status.code(), Some(1), "no pid must exit 1");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("not running"), "{stderr}");

    let _ = std::fs::remove_dir_all(&dir);
}

const TOOL_OPEN: &str = concat!("<inv", "oke name=\"bash\">");
const TOOL_CLOSE: &str = concat!("</inv", "oke>");
