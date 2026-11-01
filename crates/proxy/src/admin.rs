//! The admin surface: what `deepseek-rs rebind` and `status` talk to.
//!
//! These are ordinary API routes (they go through `authorize`), not a separate
//! privileged listener: they exist so an operator can move a thread off a
//! throttled account without editing the database by hand.
//!
//! `build_id` lives here too: it is what the admin and stats responses use to
//! say which binary is actually answering.

use axum::{extract::State, response::IntoResponse, response::Response, Json};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;

use crate::error::{backend_error, ProxyError};
use crate::sse::unix_secs;
use crate::{request, session, AppState};

/// `GET /admin/throttled` — threads whose bound account rate-limited them.
///
/// Most recent first. The account's *live* cooldown lives in the pool, not the
/// database, so it is joined here: a thread is worth rebinding when its account
/// is still cooling down, not merely because it failed once an hour ago.
pub(crate) async fn admin_throttled(State(state): State<AppState>) -> Response {
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
pub(crate) struct RebindRequest {
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

pub(crate) async fn admin_rebind(
    State(state): State<AppState>,
    Json(body): Json<RebindRequest>,
) -> Response {
    let Some((change, target_label)) = request::parse_target(body.account.as_deref()) else {
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

/// When the running executable was built, as `YYYY-MM-DD HH:MM:SS`.
///
/// There is no build script, and nothing else at startup says which binary is
/// live — which cost an hour of log archaeology once: a stale `release` build
/// from two days earlier was running, so a newly added control header was
/// silently ignored and every feature landed since was simply absent. The
/// mtime is the cheapest honest answer to "is the process I am looking at the
/// code I just wrote?".
pub(crate) fn build_id() -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
