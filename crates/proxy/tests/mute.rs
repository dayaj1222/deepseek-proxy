//! A backend mute (`user is muted`) is its own failure, not a generic one.
//!
//! DeepSeek answers HTTP 200 with a non-SSE JSON body naming a deadline. The
//! proxy must report that deadline (`429 account_muted`, `Retry-After`), hold the
//! account out of service for exactly that long, and rebind the running thread
//! off it for this turn.

mod support;

use serde_json::json;
use std::time::{SystemTime, UNIX_EPOCH};
use support::{
    complete, hello_turn, spawn_app, sse_reply, start_mock, stats_until, MockScript, Step,
};

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

fn muted_body(until: f64) -> String {
    json!({
        "code": 0, "msg": "",
        "data": {"biz_code": 5, "biz_msg": "user is muted",
                 "biz_data": {"is_muted": 1, "mute_until": until, "msg": null}}
    })
    .to_string()
}

/// A mute must be reported as `429 account_muted` with the real deadline in
/// `Retry-After`, and the account put on cooldown for exactly that long instead
/// of an exponential breaker backoff.
#[tokio::test]
async fn a_muted_account_reports_429_with_the_mute_deadline() {
    let until = now_secs() + 3600.5;
    let script = MockScript::new(vec![Step::NonSse(muted_body(until))]);
    let (upstream, _script) = start_mock(script).await;
    let app = spawn_app(&upstream, &[("a@example.com", "pw")]).await;

    // Non-streaming: the collector turns the envelope into a real status code.
    let resp = complete(&app, hello_turn()).await;
    assert_eq!(resp.status(), 429, "a mute is not a 502 backend_error");
    let body: serde_json::Value = resp.json().await.expect("json error body");
    assert_eq!(body["error"]["code"], json!("account_muted"), "{body}");
    let message = body["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("muted until") && (message.contains("3600") || message.contains("3601")),
        "the message names the deadline and the wait: {message}"
    );

    // The mute puts the account on the `rebind` list with the real cooldown.
    let listed: serde_json::Value = reqwest::get(format!("{}/admin/throttled", app.base))
        .await
        .expect("GET /admin/throttled")
        .json()
        .await
        .expect("json");
    assert_eq!(listed["count"], json!(1), "{listed}");
    assert_eq!(listed["threads"][0]["account_healthy"], json!(false));
    let cooldown_ms = listed["threads"][0]["account_cooldown_ms"]
        .as_u64()
        .unwrap();
    assert!(
        (cooldown_ms as i64 - 3_600_000).abs() < 5_000,
        "cooldown is the mute deadline, got {cooldown_ms}ms"
    );
}

/// A mute rebinds the running thread for this turn: the turn that hit the mute
/// still succeeds on another account, with the transcript replayed.
#[tokio::test]
async fn a_mute_rebinds_the_running_thread_and_replays() {
    // Turn one succeeds, so the thread has a transcript to replay. Turn two
    // hits the mute and must recover on the other account.
    let script = MockScript::new(vec![
        Step::Sse(sse_reply(2, 1, &["391"])),
        Step::NonSse(muted_body(now_secs() + 3600.0)),
        Step::Sse(sse_reply(3, 1, &["391"])),
    ]);
    let (upstream, script) = start_mock(script).await;
    let app = spawn_app(
        &upstream,
        &[("a@example.com", "pw"), ("b@example.com", "pw")],
    )
    .await;

    let first = complete(&app, hello_turn()).await;
    assert_eq!(first.status(), 200);
    assert!(first.text().await.unwrap().contains("391"));

    let resp = complete(&app, hello_turn()).await;
    assert_eq!(
        resp.status(),
        200,
        "the turn must recover on the other account"
    );
    let body = resp.text().await.unwrap();
    assert!(body.contains("391"), "{body}");
    assert!(
        !body.contains("account_muted"),
        "the mute must not reach the client when a rebind can recover: {body}"
    );
    assert_eq!(
        script.completions(),
        3,
        "two served turns, one muted attempt"
    );

    let stats = stats_until(&app, "the muted account to cool down", |s| {
        s["accounts"]
            .as_array()
            .is_some_and(|a| a.iter().any(|c| c["healthy"] == json!(false)))
    })
    .await;
    assert_eq!(stats["accounts"].as_array().unwrap().len(), 2, "{stats}");
}
