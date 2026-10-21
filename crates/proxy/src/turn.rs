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
use crate::prompt;
use crate::threads::ThreadState;
use crate::{build_prompts, completion_body, delta_messages, resolve_tools, want_thinking};
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
) -> Result<Prepared, ProxyError> {
    let existing = state.threads.resume(key);

    // Tool definitions are taught once, on the first turn of a thread. Later
    // turns ride on the server-side conversation, like the Python proxy.
    let system_extra = dialect::inject_tool_descriptions("", &req.tools);
    let (specs, required) = resolve_tools(req);

    // Image intake runs before the prompt is built because a turn carrying
    // several images degrades (first sent, rest dropped) and the model has to
    // be told what it lost. Failing the turn with a 400 instead would wedge the
    // thread: the client retries the same delta forever.
    let loaded = images::load(
        delta_messages(&req.messages),
        state.settings.image_max_bytes,
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

    let (prompt, replay) =
        build_prompts(req, existing.as_ref(), &system_extra, image_note.as_deref());
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
        "turn built"
    );
    // A brand-new thread has nothing to replay into, so a limit hit is final.
    let can_recover = existing.is_some() && !replay.is_empty();

    // Create the remote session lazily, on the first turn for this thread.
    let thread = match existing {
        Some(t) => t,
        None => {
            let conn = state.pool.route(key).await;
            let session_id = {
                let _slot = conn
                    .acquire()
                    .await
                    .map_err(|e| queue(&format!("rate slot: {e}")))?;
                conn.client()
                    .await
                    .create_chat_session()
                    .await
                    .map_err(|e| {
                        ProxyError::new(
                            format!("create_session: {e}"),
                            axum::http::StatusCode::BAD_GATEWAY,
                            "backend_error",
                        )
                    })?
            };
            let t = ThreadState {
                session_id,
                parent_message_id: None,
                prev_parents: Vec::new(),
                exchanges: 0,
            };
            tracing::info!(thread = %key, session = %t.session_id, "new remote session");
            state.threads.remember(key, t.clone());
            t
        }
    };

    let ref_file_ids = match loaded.data {
        None => Vec::new(),
        Some(bytes) => {
            let conn = state.pool.route(key).await;
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

    let thinking = want_thinking(req, state.settings.thinking_default);
    let body = completion_body(
        &thread,
        &prompt,
        thinking,
        state.settings.search_enabled,
        &ref_file_ids,
    );

    Ok(Prepared {
        thread,
        body,
        replay,
        can_recover,
        thinking,
        specs,
        required,
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

/// Silence the unused-import warning if prompt is only used for its side docs.
#[allow(dead_code)]
fn _prompt_module_is_linked(_: prompt::Prompt) {}
