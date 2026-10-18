//! Fragment routing for the DeepSeek completion SSE stream.
//!
//! Port of `_FragmentRouter` / `_route_event` from the Python proxy's
//! `deepseek_proxy/thinking.py`.
//!
//! Wire format recap:
//!
//! - The first bulk object declares fragments as `{id, type, content}` where
//!   `type` is `THINK`, `RESPONSE` or `SEARCH`.
//! - `{"o":"APPEND","p":"response/fragments","v":[{id,type,content}]}`
//!   declares a new fragment and makes it current.
//! - `{"v":"..."}` or
//!   `{"o":"APPEND","p":"response/fragments/-1/content","v":"..."}`
//!   appends to the current fragment.
//! - `BATCH` patches nest child patches whose `p` is relative to the parent.
//!
//! Routing: THINK -> reasoning, RESPONSE -> text, SEARCH -> dropped. Metadata
//! and unknown paths are never interpreted as text.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Which channel a piece of content belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    /// Answer text -> OpenAI `delta.content`.
    Text,
    /// Chain-of-thought -> OpenAI `delta.reasoning_content`.
    Reasoning,
}

/// One routed piece of content, in wire order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RerouteEvent {
    pub channel: Channel,
    pub text: String,
}

/// Tracks fragment declarations and routes content by the current type.
#[derive(Debug, Default)]
pub struct FragmentRouter {
    /// Declared fragment types in order; last is "current".
    /// `None` means "no fragment declared yet" (chunks go to text).
    types: Vec<Option<String>>,
}

impl FragmentRouter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Handle a fragment declaration list (initial bulk or APPEND list).
    pub fn declare(&mut self, fragments: &[Value]) {
        for fragment in fragments {
            let ty = fragment_type(fragment);
            self.types.push(if ty.is_empty() { None } else { Some(ty) });
        }
    }

    fn current(&self) -> Option<&str> {
        self.types.last().and_then(|t| t.as_deref())
    }

    /// Channel for `text`: `None` means drop it.
    pub fn route_content(&self, text: &str) -> Option<Channel> {
        if text.is_empty() {
            return None;
        }
        match self.current() {
            Some("THINK") => Some(Channel::Reasoning),
            Some("SEARCH") => None,
            _ => Some(Channel::Text),
        }
    }
}

fn fragment_type(fragment: &Value) -> String {
    // Python does `str(fragment.get("type", "")).upper()`. Real streams always
    // carry a string; non-string types are treated as absent here.
    fragment
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_uppercase()
}

fn fragment_content(fragment: &Value) -> &str {
    fragment.get("content").and_then(|v| v.as_str()).unwrap_or("")
}

fn looks_like_fragment_list(value: &Value) -> bool {
    let Some(items) = value.as_array() else {
        return false;
    };
    !items.is_empty()
        && items
            .iter()
            .all(|i| i.as_object().is_some_and(|o| o.contains_key("type") || o.contains_key("id")))
}

/// Route one parsed SSE event, appending to `out`.
pub fn route_event(event: &Value, router: &mut FragmentRouter, out: &mut Vec<RerouteEvent>) {
    let Some(obj) = event.as_object() else {
        return;
    };
    let o = obj.get("o").and_then(|v| v.as_str()).unwrap_or("");
    let p = obj.get("p").and_then(|v| v.as_str()).unwrap_or("");
    let v = obj.get("v");

    // BATCH: child paths are relative to the enclosing patch. Process in wire
    // order, because a fragment declaration changes the channel for later
    // deltas inside the same batch.
    if o == "BATCH" {
        if let Some(patches) = v.and_then(|v| v.as_array()) {
            for patch in patches {
                let Some(child_obj) = patch.as_object() else {
                    continue;
                };
                let child_path = child_obj.get("p").and_then(|v| v.as_str()).unwrap_or("");
                let joined = join_paths(p, child_path);
                let mut child = child_obj.clone();
                child.insert("p".into(), Value::String(joined));
                route_event(&Value::Object(child), router, out);
            }
        }
        return;
    }

    // Initial bulk object: fragments live at v.response.fragments.
    if let Some(response) = v.and_then(|v| v.get("response")) {
        if let Some(fragments) = response.get("fragments").and_then(|f| f.as_array()) {
            router.declare(fragments);
            for fragment in fragments {
                let ty = fragment_type(fragment);
                let content = fragment_content(fragment);
                if content.is_empty() {
                    continue;
                }
                match ty.as_str() {
                    "THINK" => out.push(RerouteEvent {
                        channel: Channel::Reasoning,
                        text: content.to_string(),
                    }),
                    "SEARCH" => {}
                    _ => out.push(RerouteEvent {
                        channel: Channel::Text,
                        text: content.to_string(),
                    }),
                }
            }
        }
        return;
    }

    // APPEND declaration: includes the fragment's first token.
    if o == "APPEND" && p == "response/fragments" && looks_like_fragment_list(v.unwrap_or(&Value::Null)) {
        for fragment in v.and_then(|v| v.as_array()).into_iter().flatten() {
            router.declare(std::slice::from_ref(fragment));
            let content = fragment_content(fragment);
            if let Some(channel) = router.route_content(content) {
                out.push(RerouteEvent {
                    channel,
                    text: content.to_string(),
                });
            }
        }
        return;
    }

    // Search result lists never contain reply text.
    if p.ends_with("/results") {
        return;
    }

    // Content chunk for the current fragment.
    let is_text_delta = matches!(v, Some(Value::String(_)))
        && ((o.is_empty() && p.is_empty())
            || ((o.is_empty() || o == "APPEND") && p == "response/fragments/-1/content"));

    if is_text_delta {
        let text = v.and_then(|v| v.as_str()).unwrap_or("");
        if let Some(channel) = router.route_content(text) {
            out.push(RerouteEvent {
                channel,
                text: text.to_string(),
            });
        }
    }
}

fn join_paths(parent: &str, child: &str) -> String {
    [parent, child]
        .iter()
        .map(|s| s.trim_matches('/'))
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("/")
}

/// Route a whole captured SSE stream into ordered channel events.
pub fn route_stream(raw: &str) -> Vec<RerouteEvent> {
    let mut router = FragmentRouter::new();
    let mut out = Vec::new();

    for line in raw.lines() {
        let line = line.trim();
        let Some(payload) = line.strip_prefix("data:") else {
            continue;
        };
        let payload = payload.trim();
        if payload.is_empty() || payload == "[DONE]" {
            continue;
        }
        let Ok(event) = serde_json::from_str::<Value>(payload) else {
            continue;
        };
        route_event(&event, &mut router, &mut out);
    }

    out
}

/// What one SSE line means to the caller that drives the stream.
#[derive(Debug, Clone, PartialEq)]
pub enum LineKind {
    /// Not a payload line, or a payload the caller should skip.
    Ignored,
    /// A normal event, already parsed so the caller need not parse it again.
    Event(Value),
    /// A fatal `event: hint` error sent by the upstream.
    HintError { content: String },
}

/// Classify one SSE line.
///
/// `current_event` carries the last `event:` value across calls and is updated
/// in place, exactly like the Python reader's `current_event` variable.
///
/// Python only treats a hint as fatal when it is `event: hint` and the payload
/// has `type == "error"`; everything else is a normal message.
pub fn classify_line(line: &str, current_event: &mut String) -> LineKind {
    let line = line.trim();

    if let Some(ev) = line.strip_prefix("event:") {
        *current_event = ev.trim().to_string();
        return LineKind::Ignored;
    }

    let Some(payload) = line.strip_prefix("data:") else {
        current_event.clear();
        return LineKind::Ignored;
    };

    let payload = payload.trim();
    if payload.is_empty() || payload == "[DONE]" {
        current_event.clear();
        return LineKind::Ignored;
    }

    let Ok(value) = serde_json::from_str::<Value>(payload) else {
        current_event.clear();
        return LineKind::Ignored;
    };

    let is_hint = *current_event == "hint"
        && value.get("type").and_then(|t| t.as_str()) == Some("error");
    current_event.clear();

    if is_hint {
        LineKind::HintError {
            content: value
                .get("content")
                .and_then(|c| c.as_str())
                .unwrap_or_default()
                .to_string(),
        }
    } else {
        LineKind::Event(value)
    }
}

/// True when an upstream hint says the prompt no longer fits the session.
///
/// The Python proxy keys on `input_exceeds_limit`; `context_length_exceeded`
/// is accepted too because that is what OpenAI-style clients expect.
pub fn is_context_limit(content: &str) -> bool {
    let c = content.to_lowercase();
    c.contains("input_exceeds_limit") || c.contains("context_length_exceeded")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ev() -> Vec<RerouteEvent> {
        Vec::new()
    }

    #[test]
    fn text_chunks_go_to_text() {
        let mut r = FragmentRouter::new();
        let mut out = ev();
        route_event(&json!({"v": "hello"}), &mut r, &mut out);
        route_event(&json!({"v": " world"}), &mut r, &mut out);
        assert_eq!(
            out,
            vec![
                RerouteEvent { channel: Channel::Text, text: "hello".into() },
                RerouteEvent { channel: Channel::Text, text: " world".into() },
            ]
        );
    }

    #[test]
    fn think_fragment_goes_to_reasoning() {
        let mut r = FragmentRouter::new();
        let mut out = ev();
        route_event(
            &json!({"v": {"response": {"fragments": [{"id": 1, "type": "THINK", "content": "We"}]}}}),
            &mut r,
            &mut out,
        );
        route_event(
            &json!({"o": "APPEND", "p": "response/fragments/-1/content", "v": " need"}),
            &mut r,
            &mut out,
        );
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|e| e.channel == Channel::Reasoning));
        assert_eq!(out[0].text, "We");
        assert_eq!(out[1].text, " need");
    }

    #[test]
    fn search_fragments_are_dropped() {
        let mut r = FragmentRouter::new();
        let mut out = ev();
        route_event(
            &json!({"v": {"response": {"fragments": [{"id": 1, "type": "SEARCH", "content": "query"}]}}}),
            &mut r,
            &mut out,
        );
        assert!(out.is_empty());
    }

    #[test]
    fn batch_paths_are_joined() {
        let mut r = FragmentRouter::new();
        let mut out = ev();
        route_event(
            &json!({"v": {"response": {"fragments": [{"id": 1, "type": "RESPONSE", "content": ""}]}}}),
            &mut r,
            &mut out,
        );
        route_event(
            &json!({
                "o": "BATCH",
                "p": "/response",
                "v": [{"o": "APPEND", "p": "/fragments/-1/content", "v": "hi"}]
            }),
            &mut r,
            &mut out,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].channel, Channel::Text);
        assert_eq!(out[0].text, "hi");
    }

    #[test]
    fn classify_plain_event_is_normal() {
        let mut ev = String::new();
        assert!(matches!(
            classify_line("data: {\"v\":\"hi\"}", &mut ev),
            LineKind::Event(_)
        ));
    }

    #[test]
    fn classify_hint_error_is_fatal() {
        let mut ev = String::new();
        assert_eq!(classify_line("event: hint", &mut ev), LineKind::Ignored);
        assert_eq!(
            classify_line(
                "data: {\"type\":\"error\",\"content\":\"input_exceeds_limit\"}",
                &mut ev
            ),
            LineKind::HintError {
                content: "input_exceeds_limit".into()
            }
        );
        // The event name is consumed by the hint, so the next line is normal.
        assert!(matches!(
            classify_line("data: {\"v\":\"hi\"}", &mut ev),
            LineKind::Event(_)
        ));
    }

    #[test]
    fn classify_hint_without_error_type_is_normal() {
        let mut ev = String::new();
        assert_eq!(classify_line("event: hint", &mut ev), LineKind::Ignored);
        assert!(matches!(
            classify_line("data: {\"type\":\"info\"}", &mut ev),
            LineKind::Event(_)
        ));
    }

    #[test]
    fn context_limit_is_recognised() {
        assert!(is_context_limit("Input_Exceeds_Limit: 128k"));
        assert!(is_context_limit("context_length_exceeded"));
        assert!(!is_context_limit("rate_limit_reached"));
    }

    #[test]
    fn results_lists_are_dropped() {
        let mut r = FragmentRouter::new();
        let mut out = ev();
        route_event(
            &json!({"p": "response/fragments/-1/results", "v": [{"url": "x"}]}),
            &mut r,
            &mut out,
        );
        assert!(out.is_empty());
    }
}
