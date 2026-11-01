//! The two error types: one for a failed attempt, one for what the client sees.
//!
//! `CompletionError` is internal — it names why an upstream attempt gave up.
//! `ProxyError` is the wire shape; it is built from a `CompletionError` by
//! [`error_for`] so the mapping (status, code, retry-after, the side effects of
//! a dead login) lives in exactly one place.

use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};

use crate::{pool, AppState};

/// Why a completion attempt ultimately failed.
#[derive(Debug, Clone)]
pub(crate) enum CompletionError {
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
pub(crate) struct ProxyError {
    pub(crate) message: String,
    pub(crate) status: axum::http::StatusCode,
    pub(crate) code: &'static str,
    pub(crate) param: Option<String>,
    retry_after: Option<u64>,
}

impl ProxyError {
    pub(crate) fn new(
        message: impl Into<String>,
        status: axum::http::StatusCode,
        code: &'static str,
    ) -> Self {
        Self {
            message: message.into(),
            status,
            code,
            param: None,
            retry_after: None,
        }
    }

    pub(crate) fn with_param(mut self, param: impl Into<String>) -> Self {
        self.param = Some(param.into());
        self
    }

    /// `ProxyError.payload()`: `type` follows the status, not the code.
    pub(crate) fn payload(&self) -> Value {
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

    pub(crate) fn into_response(self) -> Response {
        let retry_after = self.retry_after;
        let mut resp = (self.status, axum::Json(self.payload())).into_response();
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
pub(crate) async fn error_for(state: &AppState, key: &str, err: CompletionError) -> ProxyError {
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
