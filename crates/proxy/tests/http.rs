//! HTTP-level tests over the real router against a mock upstream.
//!
//! Every test boots the actual `deepseek-rs` binary, points it at a local mock
//! DeepSeek, and drives `/v1/chat/completions`. Nothing reaches the network and
//! no account is required, which is what T-19 exists for.

mod support;

use serde_json::json;
use support::{
    complete, complete_with_key, hello_turn, spawn_app, spawn_app_with_keys, sse_reply, start_mock,
    MockScript, Step, PLAIN_SSE,
};

/// Concatenate every delta content string in an SSE body.
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
    // The proxy answers `stream: false` with a single SSE stream today (a known
    // gap tracked in PLAN.md T-08..T-11, not this task). What must hold is that
    // the whole answer arrives and the stream terminates.
    let (upstream, _script) = start_mock(MockScript::new(vec![])).await;
    let app = spawn_app(&upstream, &[("a@example.com", "pw")]).await;

    let resp = complete(&app, hello_turn()).await;
    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();
    assert!(text.contains("[DONE]"), "{text}");
    assert!(body_text(&text).contains("391"), "{text}");
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

    let resp = complete(&app, hello_turn()).await;
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

const TOOL_OPEN: &str = concat!("<inv", "oke name=\"bash\">");
const TOOL_CLOSE: &str = concat!("</inv", "oke>");
