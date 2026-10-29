//! One interface for changing a thread's remote identity.
//!
//! A thread's identity is three things held together: the remote `session_id`,
//! the `parent_message_id` cursor ring, and the account binding. Before this
//! module four mechanisms mutated them, each remembering a different subset:
//! `turn::prepare`, `rebuild_session`, `Pool::route` and `Pool::failover`.
//!
//! [`open_session`] is now the only way a remote session is created, so the
//! field list and the persistence rule live in one place instead of four.


use std::sync::Arc;

use crate::pool::{AccountTarget, Connection};
use crate::threads::ThreadState;
use crate::{AppState, ProxyError};

/// Why a thread's remote session is being replaced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebuildReason {
    /// The backend rejected the turn as over the context limit.
    ContextLimit,
    /// The parent id was rejected and the rewind ring is exhausted (ADR-0008).
    StaleCursor,
    /// An admin route or CLI request.
    Manual,
}

impl RebuildReason {
    pub fn as_str(self) -> &'static str {
        match self {
            RebuildReason::ContextLimit => "context limit",
            RebuildReason::StaleCursor => "stale cursor",
            RebuildReason::Manual => "manual rebind",
        }
    }
}

/// What changed about the thread's identity.
pub enum ThreadChange {
    /// No prior session: the first turn on this thread.
    Fresh,
    /// A new remote session on the same account.
    Rebuild { reason: RebuildReason },
    /// A new remote session on another account.
    Rebind {
        target: AccountTarget,
        reason: RebuildReason,
    },
}

/// Create the thread's remote session and install it as the current state.
///
/// Callers pass intent, never a `ThreadState`: the field list and the
/// persistence rule live here, so a new field cannot be forgotten at one of
/// the call sites. The account is resolved first, because a rebind that cannot
/// land must fail before a session is created on the wrong account.
pub async fn open_session(
    state: &AppState,
    key: &str,
    change: ThreadChange,
) -> Result<ThreadState, ProxyError> {
    let (conn, reason) = match change {
        ThreadChange::Fresh => (state.pool().route(key).await, "new thread"),
        ThreadChange::Rebuild { reason } => (state.pool().route(key).await, reason.as_str()),
        ThreadChange::Rebind { target, reason } => {
            let conn = state
                .pool()
                .route_forced(key, &target)
                .await
                .map_err(|e| backend(&format!("rebind: {e}")))?;
            (conn, reason.as_str())
        }
    };

    let session_id = create_session(&conn).await?;
    let fresh = ThreadState::new(session_id);
    tracing::info!(
        thread = %key,
        session = %fresh.session_id,
        account = %conn.email,
        change = reason,
        "remote session opened"
    );
    state.threads.replace(key, fresh.clone());
    Ok(fresh)
}

async fn create_session(conn: &Arc<Connection>) -> Result<String, ProxyError> {
    let _slot = conn
        .acquire()
        .await
        .map_err(|e| queue(&format!("rate slot: {e}")))?;
    conn.client()
        .await
        .create_chat_session()
        .await
        .map_err(|e| {
            backend(&format!(
                "create_session: {}",
                crate::log::error_chain(&e)
            ))
        })
}

fn queue(msg: &str) -> ProxyError {
    ProxyError::new(
        msg,
        axum::http::StatusCode::SERVICE_UNAVAILABLE,
        "queue_unavailable",
    )
}

fn backend(msg: &str) -> ProxyError {
    tracing::error!(error = %msg, "opening a remote session failed");
    ProxyError::new(
        "Backend request failed",
        axum::http::StatusCode::BAD_GATEWAY,
        "backend_error",
    )
}
