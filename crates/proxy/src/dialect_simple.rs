//! Minimal tool wire format: one `<tool>` marker followed by one JSON call.
//!
//! The marker is a separator, not an XML element. Everything after it must be
//! the call JSON; there is no closing tag or trailing assistant text.

use serde_json::Value;

use crate::dialect::{MalformedCall, ScanEvent, ToolCall};

pub const TOOL_MARKER: &str = "<tool>";

pub fn render_tool_call(call: &ToolCall) -> String {
    let args = serde_json::from_str::<Value>(&call.arguments)
        .unwrap_or_else(|_| Value::String(call.arguments.clone()));
    format!(
        "{TOOL_MARKER}{{\"name\":{},\"arguments\":{}}}",
        serde_json::to_string(&call.name).unwrap_or_else(|_| "\"\"".into()),
        args
    )
}

pub fn tools_block(tools: &[Value]) -> String {
    if tools.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "Tool calls use one separator and one JSON object. For a tool call, output `<tool>` followed by `{\"name\":\"TOOL_NAME\",\"arguments\":{...}}`, with no text after the JSON. Output at most one tool call per response. Otherwise answer normally.\n\nAvailable tools:\n",
    );
    for tool in tools {
        if let Some(function) = tool.get("function") {
            out.push_str(&serde_json::to_string(function).unwrap_or_default());
            out.push('\n');
        }
    }
    out
}

pub fn reminder() -> String {
    "Tool-call reminder: for exactly one call, write `<tool>` followed by one JSON object with `name` and `arguments`; put no text after it. Do not add a closing tag.".into()
}

pub fn parse_tool_calls(text: &str) -> (String, Vec<ToolCall>) {
    let calls = scan(text)
        .into_iter()
        .filter_map(|event| match event {
            ScanEvent::Tool(call) => Some(call),
            _ => None,
        })
        .collect();
    let prose = text
        .split_once(TOOL_MARKER)
        .map_or(text, |(before, _)| before)
        .to_string();
    (prose, calls)
}

pub fn scan(text: &str) -> Vec<ScanEvent> {
    let Some((prose, body)) = text.split_once(TOOL_MARKER) else {
        return vec![ScanEvent::Text(text.to_string())];
    };
    let mut out = Vec::new();
    if !prose.is_empty() {
        out.push(ScanEvent::Text(prose.to_string()));
    }
    let raw = body.trim();
    match parse_call(raw) {
        Ok(call) => out.push(ScanEvent::Tool(call)),
        Err(name) => out.push(ScanEvent::Malformed(MalformedCall::new(
            name,
            raw.to_string(),
            "invalid_arguments_json",
        ))),
    }
    out
}

fn parse_call(raw: &str) -> Result<ToolCall, Option<String>> {
    let value: Value = serde_json::from_str(raw).map_err(|_| None)?;
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let arguments = value.get("arguments");
    let Some(name) = name else { return Err(None) };
    let Some(arguments) = arguments else {
        return Err(Some(name.to_string()));
    };
    let arguments = match arguments {
        Value::Object(_) => arguments.to_string(),
        Value::String(s) if serde_json::from_str::<Value>(s).is_ok() => s.clone(),
        _ => return Err(Some(name.to_string())),
    };
    Ok(ToolCall {
        name: name.to_string(),
        arguments,
    })
}

/// Incremental filter: stream prose before `<tool>`, but hold the call body
/// until end-of-stream so trailing text cannot accidentally be accepted.
pub struct StreamFilter {
    pending: String,
    call_body: Option<String>,
    pub calls: Vec<ToolCall>,
    pub malformed: Vec<MalformedCall>,
}

impl StreamFilter {
    pub fn new() -> Self {
        Self {
            pending: String::new(),
            call_body: None,
            calls: Vec::new(),
            malformed: Vec::new(),
        }
    }

    pub fn feed(&mut self, chunk: &str) -> String {
        if let Some(body) = &mut self.call_body {
            body.push_str(chunk);
            return String::new();
        }
        self.pending.push_str(chunk);
        if let Some((before, after)) = self.pending.split_once(TOOL_MARKER) {
            let prose = before.to_string();
            self.call_body = Some(after.to_string());
            self.pending.clear();
            return prose;
        }
        let hold = (1..TOOL_MARKER.len())
            .rev()
            .find(|&n| self.pending.ends_with(&TOOL_MARKER[..n]))
            .unwrap_or(0);
        let safe = self.pending.len() - hold;
        self.pending.drain(..safe).collect()
    }

    pub fn finish(&mut self) -> String {
        let Some(body) = self.call_body.take() else {
            return std::mem::take(&mut self.pending);
        };
        match parse_call(body.trim()) {
            Ok(call) => self.calls.push(call),
            Err(name) => self.malformed.push(MalformedCall::new(
                name,
                body.trim().to_string(),
                "invalid_arguments_json",
            )),
        }
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_single_tag_and_json_object_without_a_closing_tag() {
        assert_eq!(
            render_tool_call(&ToolCall {
                name: "bash".into(),
                arguments: "{\"command\":\"echo hi\"}".into(),
            }),
            "<tool>{\"name\":\"bash\",\"arguments\":{\"command\":\"echo hi\"}}"
        );
    }

    #[test]
    fn one_marker_routes_json_call_and_strips_it_from_prose() {
        let raw = "Working. <tool>{\"name\":\"bash\",\"arguments\":{\"command\":\"echo hi\"}}";
        let (prose, calls) = parse_tool_calls(raw);
        assert_eq!(prose, "Working. ");
        assert_eq!(
            calls,
            vec![ToolCall {
                name: "bash".into(),
                arguments: "{\"command\":\"echo hi\"}".into()
            }]
        );
    }

    #[test]
    fn trailing_text_is_malformed_and_streamed_tag_is_held() {
        assert!(matches!(
            scan("<tool>{\"name\":\"x\",\"arguments\":{}}oops").last(),
            Some(ScanEvent::Malformed(_))
        ));
        let mut filter = StreamFilter::new();
        assert_eq!(filter.feed("hello <to"), "hello ");
        assert_eq!(
            filter
                .feed("ol>{\"name\":\"bash\",\"arguments\":{}}")
                .as_str(),
            ""
        );
        assert_eq!(filter.finish(), "");
        assert_eq!(filter.calls.len(), 1);
    }
}
