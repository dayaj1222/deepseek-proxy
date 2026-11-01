//! The OpenAI request shape, its validation, and the tool/model vocabulary.
//!
//! Deserialization and validation are deliberately the same file: the serde
//! attributes say what is *accepted on the wire*, `validate_request` says what
//! is *semantically allowed*, and a reader who has to change one usually needs
//! the other. Everything here is a client error, so it all runs before the pool
//! or the thread lock is touched.

use axum::http::HeaderMap;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::ProxyError;
use crate::header_value;

#[derive(Debug, Deserialize)]
pub(crate) struct ChatRequest {
    #[serde(default)]
    pub(crate) model: Option<String>,
    /// Explicit conversation key, mirroring the Python proxy's `thread_id`.
    #[serde(default)]
    pub(crate) thread_id: Option<String>,
    #[serde(default)]
    pub(crate) messages: Vec<Msg>,
    #[serde(default)]
    pub(crate) stream: bool,
    #[serde(default)]
    pub(crate) reasoning_effort: Option<String>,
    #[serde(default)]
    pub(crate) thinking: Option<Value>,
    #[serde(default)]
    pub(crate) reasoning: Option<Value>,
    #[serde(default)]
    pub(crate) tools: Vec<Value>,
    /// OpenAI `tool_choice`: "none" drops every tool, "required" forces a call,
    /// an object narrows to one named function.
    #[serde(default)]
    pub(crate) tool_choice: Option<Value>,
    /// OpenAI `parallel_tool_calls`; defaults to true like the OpenAI API.
    #[serde(default = "default_true")]
    pub(crate) parallel_tool_calls: bool,
    /// OpenAI `stream_options`; only `include_usage` is honoured.
    #[serde(default)]
    pub(crate) stream_options: Option<Value>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize)]
pub(crate) struct Msg {
    pub(crate) role: String,
    #[serde(default)]
    pub(crate) content: Option<Value>,
    // Deserialized for completeness; the prompt builders only need role/content.
    #[serde(default)]
    pub(crate) tool_calls: Vec<Value>,
    /// Tool-call id this result answers, rendered as `Tool result (id=...)`.
    #[serde(default)]
    pub(crate) tool_call_id: Option<String>,
}

/// How `tool_choice` narrows the offered tools and sets the `required` flag.
///
/// Port of the filtering at the top of `ChatService.events()`:
/// `"none"` drops every tool, a named choice keeps only that function and
/// implies required, and `"required"` keeps all tools and implies required.
pub(crate) fn resolve_tools(req: &ChatRequest) -> (Vec<crate::dialect::ToolSpec>, bool) {
    let mut tools = req.tools.clone();
    let mut required = false;
    match req.tool_choice.as_ref() {
        Some(Value::String(s)) if s == "none" => tools.clear(),
        Some(Value::String(s)) if s == "required" => required = true,
        Some(Value::Object(obj)) => {
            if let Some(name) = obj
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
            {
                tools.retain(|t| {
                    t.get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(|n| n.as_str())
                        == Some(name)
                });
                required = true;
            }
        }
        _ => {}
    }
    (crate::dialect::tool_specs(&tools), required)
}

/// `x-deepseek-rs-rebind`: `next`, `fresh`, or an account email.
///
/// This is the deterministic channel a pi extension uses: the extension eats
/// the operator's sentinel in the `input` hook, so the sentinel never becomes a
/// message, and sets this header on the request that follows. The proxy then
/// never scans message text for directives, and nothing has to be stripped
/// from the transcript.
pub(crate) fn rebind_header(
    headers: &HeaderMap,
) -> Result<Option<crate::session::ThreadChange>, ProxyError> {
    let Some(raw) = header_value(headers, "x-deepseek-rs-rebind") else {
        return Ok(None);
    };
    // An unknown target is a client error: a silent no-op would leave the
    // caller believing the session moved.
    let (change, _) = parse_target(Some(&raw)).ok_or_else(|| {
        ProxyError::new(
            "x-deepseek-rs-rebind must be next, fresh, or an account email",
            axum::http::StatusCode::BAD_REQUEST,
            "invalid_request",
        )
        .with_param("x-deepseek-rs-rebind")
    })?;
    Ok(Some(change))
}

/// Resolve a rebind target: `next`, `fresh`/`current`, or an account email.
///
/// Returns the change plus the label to report back, so the HTTP handler and the
/// header path agree on what a target means.
pub(crate) fn parse_target(
    account: Option<&str>,
) -> Option<(crate::session::ThreadChange, String)> {
    use crate::pool::AccountTarget;
    use crate::session::{RebuildReason, ThreadChange};

    let raw = account.unwrap_or("next").trim();
    match raw {
        "" | "next" => Some((
            ThreadChange::Rebind {
                target: AccountTarget::Next,
                reason: RebuildReason::Manual,
            },
            "next".to_string(),
        )),
        "fresh" | "current" => Some((
            ThreadChange::Rebuild {
                reason: RebuildReason::Manual,
            },
            "fresh".to_string(),
        )),
        email if email.contains('@') => Some((
            ThreadChange::Rebind {
                target: AccountTarget::Email(email.to_string()),
                reason: RebuildReason::Manual,
            },
            email.to_string(),
        )),
        _ => None,
    }
}

/// Port of `ChatService.validate()`'s request-shape checks (the pydantic model
/// plus the tool-schema pass). Everything here is a client error: the handler
/// returns it before touching the pool or the thread lock.
pub(crate) fn validate_request(req: &ChatRequest) -> Result<(), ProxyError> {
    // `model` is optional here: the handler defaults it from config, which the
    // Python proxy does not allow but this port documents deliberately.
    if req.messages.is_empty() {
        return Err(invalid("messages", "List should have at least 1 item"));
    }
    if let Some(id) = &req.thread_id {
        if id.is_empty() || id.chars().count() > 256 {
            return Err(invalid("thread_id", "Invalid thread id"));
        }
    }
    // `tool_choice`: only auto/none/required or a named-function object.
    if let Some(choice) = &req.tool_choice {
        let ok = match choice {
            Value::String(s) => matches!(s.as_str(), "auto" | "none" | "required"),
            Value::Object(obj) => obj
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .is_some_and(|n| !n.is_empty()),
            _ => false,
        };
        if !ok {
            return Err(invalid(
                "tool_choice",
                "Input should be a valid tool choice",
            ));
        }
    }
    for (i, m) in req.messages.iter().enumerate() {
        let at = |field: &str| format!("messages.{i}.{field}");
        if m.role == "tool" && m.tool_call_id.as_deref().unwrap_or_default().is_empty() {
            return Err(invalid(
                &at("tool_call_id"),
                "Tool messages require tool_call_id",
            ));
        }
        // Assistant messages may carry only tool_calls; every other role needs content.
        let content_missing = match &m.content {
            None => true,
            Some(Value::String(s)) => s.is_empty(),
            Some(Value::Array(parts)) => parts.is_empty(),
            Some(_) => false,
        };
        if content_missing && !(m.role == "assistant" && !m.tool_calls.is_empty()) {
            return Err(invalid(
                &at("content"),
                "Message requires content or assistant tool_calls",
            ));
        }
        if let Some(Value::Array(parts)) = &m.content {
            for (j, part) in parts.iter().enumerate() {
                let ok = match part.get("type").and_then(|t| t.as_str()) {
                    Some("text") => part.get("text").and_then(|t| t.as_str()).is_some(),
                    Some("image_url") => {
                        m.role == "user"
                            && part
                                .get("image_url")
                                .and_then(|i| i.get("url"))
                                .and_then(|u| u.as_str())
                                .is_some()
                    }
                    _ => false,
                };
                if !ok {
                    return Err(invalid(
                        &at(&format!("content.{j}")),
                        "Unsupported or invalid message content part",
                    ));
                }
            }
        }
    }
    for tool in &req.tools {
        let Some(func) = tool.get("function") else {
            return Err(invalid("tools", "Invalid or unsupported tool schema"));
        };
        let name = func
            .get("name")
            .and_then(|n| n.as_str())
            .unwrap_or_default();
        if name.is_empty() || name.chars().count() > 128 {
            return Err(invalid("tools", "Invalid or unsupported tool schema"));
        }
        let parameters = func.get("parameters").cloned().unwrap_or_else(|| json!({}));
        if !parameters.is_object() {
            return Err(invalid("tools", "Invalid or unsupported tool schema"));
        }
        // Python rejects the schema itself, and refuses remote `$ref`s so no
        // network fetch can happen during validation.
        if has_external_ref(&parameters) || jsonschema::validator_for(&parameters).is_err() {
            return Err(invalid("tools", "Invalid or unsupported tool schema"));
        }
    }
    Ok(())
}

fn invalid(param: &str, message: &str) -> ProxyError {
    ProxyError::new(
        message,
        axum::http::StatusCode::BAD_REQUEST,
        "invalid_request",
    )
    .with_param(param)
}

/// True when the schema contains a `$ref`/`$dynamicRef` that points outside the
/// document. Port of the recursive check in `ChatService.validate()`.
fn has_external_ref(node: &Value) -> bool {
    match node {
        Value::Object(map) => map.iter().any(|(k, v)| {
            (k == "$ref" || k == "$dynamicRef")
                .then(|| !v.as_str().unwrap_or_default().starts_with('#'))
                .unwrap_or(false)
                || has_external_ref(v)
        }),
        Value::Array(items) => items.iter().any(has_external_ref),
        _ => false,
    }
}

/// `ChatService.validate()`'s supported-model set, case-insensitive.
pub(crate) fn is_supported_model(model: &str) -> bool {
    matches!(
        model.to_ascii_uppercase().as_str(),
        "DEFAULT" | "EXPERT" | "VISION" | "DEEPSEEK-CHAT" | "DEEPSEEK-V3"
    )
}
