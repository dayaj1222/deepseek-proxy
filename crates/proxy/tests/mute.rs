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
    complete, hello_turn, spawn_app, spawn_app_with_env, sse_reply, start_mock, stats_until,
    MockScript, Step,
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

/// A mute must be reported as `429 account_muted`, with the wait in days and
/// hours, and the account put on cooldown for exactly that long instead of an
/// exponential breaker backoff.
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
    // A one-hour mute: the deadline is floored to whole seconds and `now` has
    // moved on by the time the message is built, so the remainder can land just
    // under the hour. Accept either side of that boundary rather than pinning a
    // value that depends on how fast the test machine is.
    assert!(
        message.contains("muted") && (message.contains("1h 0m") || message.contains("59m")),
        "the message says how long the wait is, in hours and minutes: {message}"
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

/// A mute longer than a day leaves the account out for that whole deadline, not
/// just until a breaker backoff would have expired, and the next turn must not
/// route back to it.
#[tokio::test]
async fn a_mute_longer_than_a_day_holds_the_account_out_for_the_deadline() {
    let three_days = now_secs() + 3.0 * 86_400.0;
    let script = MockScript::new(vec![
        Step::NonSse(muted_body(three_days)),
        Step::Sse(sse_reply(4, 1, &["391"])),
    ]);
    let (upstream, _script) = start_mock(script).await;
    let app = spawn_app(
        &upstream,
        &[("a@example.com", "pw"), ("b@example.com", "pw")],
    )
    .await;

    let resp = complete(&app, hello_turn()).await;
    assert_eq!(resp.status(), 429, "the first turn still reports the mute");
    let body: serde_json::Value = resp.json().await.expect("json error body");
    let message = body["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("3d 0h"),
        "a three-day mute reads as days and hours, not seconds: {message}"
    );

    // Three days is the recorded deadline, so the account is out of rotation for
    // exactly that long — `/admin/throttled` reports it as unhealthy and shows
    // the cooldown, which is what `rebind` uses.
    let listed: serde_json::Value = reqwest::get(format!("{}/admin/throttled", app.base))
        .await
        .expect("GET /admin/throttled")
        .json()
        .await
        .expect("json");
    assert_eq!(listed["threads"][0]["account_healthy"], json!(false));
    let cooldown_ms = listed["threads"][0]["account_cooldown_ms"]
        .as_u64()
        .expect("a three-day mute has a cooldown");
    assert!(
        (cooldown_ms as i64 - 3 * 86_400_000).abs() < 60_000,
        "cooldown is the whole mute deadline, got {cooldown_ms}ms"
    );

    // A new thread must land on the other account, never the muted one.
    let second = complete(&app, hello_turn()).await;
    assert_eq!(
        second.status(),
        200,
        "the remaining account must serve new threads"
    );
    assert!(second.text().await.unwrap().contains("391"));

    let stats = stats_until(&app, "the muted account to be held out", |s| {
        s["accounts"]
            .as_array()
            .is_some_and(|a| a.iter().filter(|c| c["healthy"] == json!(false)).count() == 1)
    })
    .await;
    assert_eq!(stats["accounts"].as_array().unwrap().len(), 2, "{stats}");
}

/// An expired mute comes back on its own: the sweep clears the deadline and the
/// reloaded pool serves the account again, with no `accounts enable`.
#[tokio::test]
async fn an_expired_mute_returns_the_account_to_rotation() {
    // The mute is a few seconds out, so the account really is unhealthy when the
    // turn returns; the sweep interval is shortened below so the test does not
    // wait the production 30s. Without both of those the account would look
    // healthy from the start and the assertions would pass vacuously.
    let script = MockScript::new(vec![
        Step::NonSse(muted_body(now_secs() + 3.0)),
        Step::Sse(sse_reply(5, 1, &["391"])),
    ]);
    let (upstream, _script) = start_mock(script).await;
    let app = spawn_app_with_env(
        &upstream,
        &[("a@example.com", "pw")],
        &[("MUTE_SWEEP_SECS", "1")],
    )
    .await;

    let resp = complete(&app, hello_turn()).await;
    assert_eq!(resp.status(), 429);

    // The account is out until the deadline plus the 15s grace.
    let listed: serde_json::Value = reqwest::get(format!("{}/admin/throttled", app.base))
        .await
        .expect("GET /admin/throttled")
        .json()
        .await
        .expect("json");
    assert_eq!(
        listed["threads"][0]["account_healthy"],
        json!(false),
        "the account must be out of rotation before the sweep runs"
    );

    let stats = stats_until(&app, "the mute to expire and reload", |s| {
        s["accounts"]
            .as_array()
            .is_some_and(|a| a.iter().any(|c| c["healthy"] == json!(true)))
    })
    .await;
    assert_eq!(stats["accounts"].as_array().unwrap().len(), 1, "{stats}");

    let after = complete(&app, hello_turn()).await;
    assert_eq!(after.status(), 200, "the account must serve again");
    assert!(after.text().await.unwrap().contains("391"));
}
