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
