//! Turn preparation: everything a turn does before the upstream stream opens.
//!
//! A **turn** is one client request and all the work behind it. Preparation is
//! the half that has no streaming in it: resume the thread, decide what the
//! model is told, make sure a remote session exists, upload the image, and
//! build the completion body. The streaming half stays in `main` because its
//! output is an axum `Response`.
//!
//! Extracting this is what gives the cursor rules one locality: `resume`,
//! `remember` and the rebuild path all live on `threads::Threads`.

use client::CompletionBody;
use serde_json::Value;

use crate::dialect;
use crate::images;
use crate::threads::ThreadState;
use crate::{completion_body, delta_messages, resolve_tools, want_thinking};
use crate::{AppState, ChatRequest, ProxyError};

/// Everything the streaming half needs, or a request-shaped error.
pub struct Prepared {
    /// The live thread, after the lazy session create.
    pub thread: ThreadState,
    /// Upstream body, with the parent id and any uploaded file ids.
    pub body: CompletionBody,
    pub replay: String,
    /// True when the thread existed and has a transcript, so a limit hit can be
    /// recovered by rebuilding the session and replaying it.
    pub can_recover: bool,
    pub thinking: bool,
    pub specs: Vec<dialect::ToolSpec>,
    pub required: bool,
    /// Wire dialect this turn speaks (snapshot at turn start).
    pub dialect: dialect::Dialect,
}

/// Prepare a turn: resume, prompts, session, image, body.
///
/// Errors are `ProxyError` because every failure here is the client's to see:
/// an invalid image, a turn with nothing to send, or an account queue that is
/// full. Nothing is streamed yet, so no partial output can be orphaned.
pub async fn prepare(
    state: &AppState,
    key: &str,
    req: &ChatRequest,
    model: &str,
    rebind: Option<crate::session::ThreadChange>,
) -> Result<Prepared, ProxyError> {
    let existing = state.threads.resume(key);
    // A rebind opens a session that has never seen this conversation, so the
    // turn must carry the whole transcript. Building the prompt as if this were
    // a continuation would send only the delta into an empty session and lose
    // everything said before it.
    let rebinding = rebind.is_some();
    let wire = state.settings.get().dialect;

    // Tool definitions are taught once, on the first turn of a thread. Later
    // turns ride on the server-side conversation, like the Python proxy.
    let system_extra = wire.system_extra(&req.tools);
    let (specs, required) = resolve_tools(req);

    // Image intake runs before the prompt is built because a turn carrying
    // several images degrades (first sent, rest dropped) and the model has to
    // be told what it lost. Failing the turn with a 400 instead would wedge the
    // thread: the client retries the same delta forever.
    let loaded = images::load(
        delta_messages(&req.messages),
        state.settings.get().image_max_bytes,
    )
    .await
    .map_err(|e| {
        tracing::warn!(thread = %key, error = %e, "rejected image");
        ProxyError::new(e.0, axum::http::StatusCode::BAD_REQUEST, "invalid_image")
            .with_param("messages")
    })?;
    let image_note = loaded.note();
    if image_note.is_some() {
        state
            .stats
            .images_dropped
            .fetch_add(loaded.dropped as u64, std::sync::atomic::Ordering::Relaxed);
        tracing::warn!(
            thread = %key,
            total = loaded.total,
            dropped = loaded.dropped,
            "dropped extra images; sending the first only"
        );
    }

    let (prompt, replay) = crate::build_prompts_with(
        req,
        if rebinding { None } else { existing.as_ref() },
        &system_extra,
        image_note.as_deref(),
        wire,
    );
    if prompt.is_empty() {
        tracing::warn!(thread = %key, "rejecting turn: no user message or tool result");
        return Err(ProxyError::new(
            "no user message or tool result in continuation turn",
            axum::http::StatusCode::BAD_REQUEST,
            "invalid_request",
        ));
    }
    tracing::debug!(
        thread = %key,
        fresh = existing.is_none(),
        prompt_chars = prompt.len(),
        replay_chars = replay.len(),
        dialect = wire.name(),
        "turn built"
    );
    // A brand-new thread has nothing to replay into, so a limit hit is final.
    // A rebound thread does have a transcript, and that is exactly what was
    // just replayed, so it recovers like any existing thread.
    let can_recover = (existing.is_some() || rebinding) && !replay.is_empty();

    // Create the remote session lazily, on the first turn for this thread, or
    // immediately when this turn is itself a rebind.
    let thread = match rebind {
        Some(change) => crate::session::open_session(state, key, change).await?,
        None => match existing {
            Some(t) => t,
            None => {
                crate::session::open_session(state, key, crate::session::ThreadChange::Fresh)
                    .await?
            }
        },
    };

    let ref_file_ids = match loaded.data {
        None => Vec::new(),
        Some(bytes) => {
            let conn = state.pool().route(key).await;
            let _slot = conn
                .acquire()
                .await
                .map_err(|e| queue(&format!("rate slot: {e}")))?;
            match conn.client().await.upload_image(&bytes, model).await {
                Ok(uploaded) => {
                    tracing::info!(
                        thread = %key,
                        file_id = %uploaded.file_id,
                        bytes = uploaded.size_bytes,
                        width = uploaded.width,
                        height = uploaded.height,
                        "image uploaded"
                    );
                    vec![uploaded.file_id]
                }
                // Python degrades to text-only rather than failing the turn.
                Err(e) => {
                    tracing::warn!(thread = %key, error = %e, "image upload failed; continuing text-only");
                    Vec::new()
                }
            }
        }
    };

    let thinking = want_thinking(req, state.settings.get().thinking_default);
    let body = completion_body(
        &thread,
        &prompt,
        thinking,
        state.settings.get().search_enabled,
        &ref_file_ids,
        model,
    );

    Ok(Prepared {
        thread,
        body,
        replay,
        can_recover,
        thinking,
        specs,
        required,
        dialect: wire,
    })
}

fn queue(message: &str) -> ProxyError {
    ProxyError::new(
        message.to_string(),
        axum::http::StatusCode::SERVICE_UNAVAILABLE,
        "queue_unavailable",
    )
}

/// Whether the proxy advertises this model id.
///
/// Kept next to preparation because a rejected model must never take the
/// thread lock or create a remote session.
pub fn accepts_model(model: &str) -> bool {
    crate::is_supported_model(model)
}

/// `stream_options.include_usage`, defaulted to false.
pub fn include_usage(options: Option<&Value>) -> bool {
    options
        .and_then(|o| o.get("include_usage"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}
