//! Tool-call wire format: teaching text and parsing.
//!
//! Port of the parts of `deepseek_proxy/tools/format.py` that matter for a
//! working proxy: the `json_invoke` dialect (what pi speaks), prompt
//! injection, and extraction of `<invoke name="t">{...}</invoke>` blocks from
//! model output.
//!
//! Not ported: the repair/healing machinery for malformed blocks, and the
//! `xml_params` / legacy `dsml` dialects. Only the json_invoke spelling is
//! parsed; anything else falls through as plain text.

/// A parsed tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    pub name: String,
    /// JSON arguments, already parsed.
    pub arguments: serde_json::Value,
}

pub const INVOKE_OPEN: &str = "<invoke";
pub const INVOKE_CLOSE: &str = "</invoke>";

/// Concrete example shown to the model.
pub const TEMPLATE: &str =
    "<invoke name=\"tool_name\">{\"param1\": \"value1\", \"count\": 5}</invoke>";

const PROTOCOL_HEADER: &str = "## TOOL CALL PROTOCOL — MANDATORY, STRICT FORMAT";
const TOOLS_HEADER: &str = "## Available Tools";

fn tool_section(name: &str, description: &str, params: &serde_json::Value) -> String {
    let params = serde_json::to_string_pretty(params).unwrap_or_else(|_| "{}".into());
    format!(
        "### {name}\nDescription: {description}\nParameters (JSON Schema):\n```json\n{params}\n```"
    )
}

fn instruction() -> String {
    let mut s = String::new();
    s.push_str("To call a tool, emit a block in EXACTLY this format:\n\n");
    s.push_str(TEMPLATE);
    s.push_str("\n\nThe tool name goes in the name attribute of the opening tag. Between the\n");
    s.push_str("tags put ONE JSON object holding all arguments. Each property in a tool's\n");
    s.push_str("Parameters (JSON Schema) below corresponds to one key of that object.\n\n");
    s.push_str("### Required tool-call rules\n");
    s.push_str("1. ATTRIBUTES: the opening tag MUST carry a name attribute:\n");
    s.push_str("   <invoke name=\"tool_name\">. Extra attributes are ignored, but name\n");
    s.push_str("   is required.\n");
    s.push_str("2. BODY: exactly one JSON object between the tags. Strings, numbers,\n");
    s.push_str("   booleans, null, arrays and nested objects are all fine as JSON values.\n");
    s.push_str("   Do NOT wrap the block in code fences, never emit bare JSON with no tags.\n");
    s.push_str("3. LITERAL MARKERS: if a value must contain the literal text <invoke\n");
    s.push_str("   or </invoke>, write a backslash directly before it.\n");
    s.push_str("4. MULTIPLE CALLS: you may emit several complete blocks back to back IN ONE\n");
    s.push_str("   TURN ONLY for short, independent, read-only calls. NEVER batch writes,\n");
    s.push_str("   edits, long-running commands, or complex/stateful operations.\n");
    s.push_str("5. You may write normal text before or after tool call blocks.\n");
    s.push_str("6. CALL ONLY WHEN NEEDED: if no tool fits, answer directly with no block.\n");
    s.push_str("7. NO GUESSING: use only parameters from the schema below.");
    s
}

fn final_reminder() -> String {
    let mut s = String::new();
    s.push_str("FINAL REMINDER: tool calls use exactly this form:\n\n");
    s.push_str(TEMPLATE);
    s.push_str("\n\nname in the <invoke attribute; one JSON object between the tags.\n");
    s.push_str("Emit each tool call exactly once. Never repeat an opening tag, closing tag,\n");
    s.push_str("or the complete tool-call block after it has been emitted.");
    s
}

/// Periodic re-anchoring reminder, emitted every N user/tool messages.
///
/// Port of `format_reminder_text()` in `deepseek_proxy/tools/format.py`, with
/// the warning sharpened: it names the concrete failure modes seen in
/// practice (legacy DSML markers leaking into the reply, a doubled closing
/// tag, prose or a code fence inside the block) instead of only describing
/// the happy path.
pub fn periodic_reminder() -> String {
    let mut s = String::new();
    s.push_str("⚠ HARD FORMAT REQUIREMENT — READ BEFORE EMITTING ANY TOOL CALL.\n");
    s.push_str("Violations are NOT repaired for you: a malformed or repeated block is DROPPED\n");
    s.push_str("and never executed, and you will burn this turn on a correction loop instead\n");
    s.push_str("of progressing the task.\n");
    s.push_str("Reminder — tool calls use exactly this form:\n\n");
    s.push_str(TEMPLATE);
    s.push_str("\n\n!!IMPORTANT!!\n");
    s.push_str("1. Never wrap or decorate a block with legacy DSML marker tags. They are\n");
    s.push_str("   not part of the format, are dropped, and are shown to the user as junk.\n");
    s.push_str("2. Never emit the closing tag twice, and never repeat a whole block after\n");
    s.push_str("   it has already been emitted.\n");
    s.push_str("3. The body must be one JSON object: no code fence, no trailing prose,\n");
    s.push_str("   no bare JSON without tags, and a missing closing tag discards the call.\n");
    s.push_str("Get the format wrong and your tool call silently does nothing.");
    s
}

/// Append the tool protocol + tool definitions to a system prompt.
///
/// Returns `system_prompt` unchanged when `tools` is empty, matching Python.
pub fn inject_tool_descriptions(system_prompt: &str, tools: &[serde_json::Value]) -> String {
    if tools.is_empty() {
        return system_prompt.to_string();
    }

    let mut sections = Vec::new();
    for tool in tools {
        if tool.get("type").and_then(|t| t.as_str()) != Some("function") {
            continue;
        }
        let Some(func) = tool.get("function") else {
            continue;
        };
        let Some(name) = func.get("name").and_then(|n| n.as_str()) else {
            continue;
        };
        let params = func
            .get("parameters")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        let description = func
            .get("description")
            .and_then(|d| d.as_str())
            .unwrap_or("No description provided.");
        sections.push(tool_section(name, description, &params));
    }

    let tools_block = sections.join("\n\n");
    format!(
        "{system_prompt}\n\n---\n{PROTOCOL_HEADER}\n\n{}\n\n{TOOLS_HEADER}\n\n{tools_block}\n---\n{}",
        instruction(),
        final_reminder()
    )
}
/// Extract `<invoke name="t">{json}</invoke>` blocks from model text.
///
/// Returns calls in order. A block whose body is not a JSON object is skipped
/// rather than mis-parsed — better to lose one call than send garbage args.
pub fn extract_tool_calls(text: &str) -> Vec<ToolCall> {
    let mut out = Vec::new();
    let mut rest = text;

    while let Some(start) = rest.find(INVOKE_OPEN) {
        let after_open = &rest[start + INVOKE_OPEN.len()..];

        // Require the next char to be whitespace, so "<invokex" cannot match.
        let Some(first) = after_open.chars().next() else {
            break;
        };
        if !first.is_whitespace() {
            rest = after_open;
            continue;
        }

        let Some(tag_end) = after_open.find('>') else {
            break;
        };
        let attrs = &after_open[..tag_end];

        let body_and_rest = &after_open[tag_end + 1..];
        let Some(close_rel) = body_and_rest.find(INVOKE_CLOSE) else {
            break;
        };
        let body = body_and_rest[..close_rel].trim();

        if let Some(name) = attr_value(attrs, "name") {
            if let Ok(serde_json::Value::Object(map)) =
                serde_json::from_str::<serde_json::Value>(body)
            {
                out.push(ToolCall {
                    name,
                    arguments: serde_json::Value::Object(map),
                });
            }
        }

        rest = &body_and_rest[close_rel + INVOKE_CLOSE.len()..];
    }

    out
}

fn attr_value(attrs: &str, key: &str) -> Option<String> {
    let needle = format!("{key}=");
    let idx = attrs.find(&needle)?;
    let after = &attrs[idx + needle.len()..];
    let quote = after.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let inner = &after[1..];
    let end = inner.find(quote)?;
    Some(inner[..end].to_string())
}

/// Remove tool-call blocks, leaving the prose around them.
pub fn strip_tool_calls(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find(INVOKE_OPEN) {
        out.push_str(&rest[..start]);
        let after = &rest[start..];
        match after.find(INVOKE_CLOSE) {
            Some(close) => rest = &after[close + INVOKE_CLOSE.len()..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extracts_single_call() {
        let text = "Sure.\n<invoke name=\"bash\">{\"command\": \"ls\"}</invoke>";
        let calls = extract_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "bash");
        assert_eq!(calls[0].arguments, json!({"command": "ls"}));
    }

    #[test]
    fn extracts_multiple_calls() {
        let text = concat!(
            "<invoke name=\"a\">{\"x\": 1}</invoke>\n",
            "<invoke name=\"b\">{\"y\": [1,2]}</invoke>"
        );
        let calls = extract_tool_calls(text);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].name, "b");
        assert_eq!(calls[1].arguments, json!({"y": [1, 2]}));
    }

    #[test]
    fn skips_malformed_body() {
        assert!(extract_tool_calls("<invoke name=\"b\">not json</invoke>").is_empty());
    }

    #[test]
    fn skips_missing_name() {
        assert!(extract_tool_calls("<invoke>{\"x\": 1}</invoke>").is_empty());
    }

    #[test]
    fn does_not_match_invokex() {
        assert!(extract_tool_calls("<invokex name=\"a\">{\"x\": 1}</invokex>").is_empty());
    }

    #[test]
    fn strip_leaves_prose() {
        let text = "before <invoke name=\"t\">{\"a\":1}</invoke> after";
        let stripped = strip_tool_calls(text);
        assert!(stripped.contains("before"));
        assert!(stripped.contains("after"));
        assert!(!stripped.contains("invoke"));
    }

    #[test]
    fn inject_noop_without_tools() {
        assert_eq!(inject_tool_descriptions("sys", &[]), "sys");
    }

    #[test]
    fn inject_adds_protocol_and_tools() {
        let tools = vec![json!({
            "type": "function",
            "function": {
                "name": "bash",
                "description": "run a command",
                "parameters": {"type": "object"}
            }
        })];
        let out = inject_tool_descriptions("BASE", &tools);
        assert!(out.starts_with("BASE"));
        assert!(out.contains("TOOL CALL PROTOCOL"));
        assert!(out.contains("### bash"));
        assert!(out.contains("FINAL REMINDER"));
    }
}
/// Incremental filter that separates tool-call blocks from prose in a
/// streamed text channel.
///
/// `<invoke ...>{json}</invoke>` blocks can arrive split across chunks, so
/// text is buffered until a block is complete (or provably absent). Text that
/// precedes an opening tag is released immediately; a trailing partial tag
/// (e.g. `...<inv`) is held back so it is never emitted half-formed.
pub struct StreamToolFilter {
    pending: String,
    /// Completed calls, in discovery order.
    pub calls: Vec<ToolCall>,
}

impl Default for StreamToolFilter {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamToolFilter {
    pub fn new() -> Self {
        Self {
            pending: String::new(),
            calls: Vec::new(),
        }
    }

    /// Feed a text chunk; returns prose that is safe to emit as content now.
    pub fn feed(&mut self, text: &str) -> String {
        self.pending.push_str(text);
        let mut out = String::new();

        loop {
            match self.pending.find(INVOKE_OPEN) {
                None => {
                    // Hold back a trailing partial marker like "<inv".
                    let keep = partial_marker_suffix(&self.pending);
                    let split = self.pending.len() - keep;
                    out.push_str(&self.pending[..split]);
                    self.pending.drain(..split);
                    break;
                }
                Some(start) => {
                    let after = &self.pending[start..];
                    match after.find(INVOKE_CLOSE) {
                        Some(close) => {
                            // Complete block: parse and consume it.
                            out.push_str(&self.pending[..start]);
                            let block = after[..close + INVOKE_CLOSE.len()].to_string();
                            self.calls.extend(extract_tool_calls(&block));
                            self.pending.drain(..start + close + INVOKE_CLOSE.len());
                        }
                        None => {
                            // Incomplete block: emit everything before it.
                            out.push_str(&self.pending[..start]);
                            self.pending.drain(..start);
                            break;
                        }
                    }
                }
            }
        }

        out
    }

    /// Flush at end of stream. An unterminated block degrades to text.
    pub fn finish(&mut self) -> String {
        let rest = std::mem::take(&mut self.pending);
        // Only a *complete* block would have been parsed; anything left is prose.
        rest
    }
}

/// Length of the suffix of `s` that is a proper prefix of `<invoke`.
fn partial_marker_suffix(s: &str) -> usize {
    let bytes = s.as_bytes();
    let marker = INVOKE_OPEN.as_bytes();
    let max = marker.len().saturating_sub(1).min(bytes.len());
    for len in (1..=max).rev() {
        if bytes[bytes.len() - len..] == marker[..len] {
            return len;
        }
    }
    0
}

#[cfg(test)]
mod stream_tests {
    use super::*;

    #[test]
    fn passes_plain_text_through() {
        let mut f = StreamToolFilter::new();
        assert_eq!(f.feed("hello "), "hello ");
        assert_eq!(f.feed("world"), "world");
        assert_eq!(f.finish(), "");
        assert!(f.calls.is_empty());
    }

    #[test]
    fn extracts_block_split_across_chunks() {
        let mut f = StreamToolFilter::new();
        assert_eq!(f.feed("Sure. <inv"), "Sure. ");
        assert_eq!(f.feed("oke name=\"bash\">{\"command\":"), "");
        assert_eq!(f.feed("\"ls\"}</invoke>"), "");
        assert_eq!(f.calls.len(), 1);
        assert_eq!(f.calls[0].name, "bash");
        assert_eq!(f.calls[0].arguments, serde_json::json!({"command": "ls"}));
    }

    #[test]
    fn holds_back_partial_tag_until_finish() {
        let mut f = StreamToolFilter::new();
        assert_eq!(f.feed("text <inv"), "text ");
        // "<invoke" is still an incomplete opening tag: hold it, do not emit.
        assert_eq!(f.feed("oke"), "");
        // Only at end of stream does it degrade to text.
        assert_eq!(f.finish(), "<invoke");
    }

    #[test]
    fn unterminated_block_becomes_text_at_finish() {
        let mut f = StreamToolFilter::new();
        assert_eq!(f.feed("a <invoke name=\"x\">{"), "a ");
        assert_eq!(f.finish(), "<invoke name=\"x\">{");
        assert!(f.calls.is_empty());
    }

    #[test]
    fn prose_after_block_is_emitted() {
        let mut f = StreamToolFilter::new();
        let out = f.feed("<invoke name=\"t\">{\"a\":1}</invoke> done");
        assert_eq!(out, " done");
        assert_eq!(f.calls.len(), 1);
    }
}
