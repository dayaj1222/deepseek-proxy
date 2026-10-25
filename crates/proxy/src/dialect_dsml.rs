//! The DeepSeek V4 `｜DSML｜` wire dialect.
//!
//! Source of truth rendered byte-for-byte:
//! `ggml-org/llama.cpp` -> `models/templates/deepseek-ai-DeepSeek-V4.jinja`.
//! Every literal below is copied from there; spacing is load-bearing.
//!
//! Critical characters:
//! - `｜` is U+FF5C FULLWIDTH VERTICAL LINE (never ASCII `|`).
//! - `▁` is U+2581 LOWER ONE EIGHTH BLOCK (never `_` or a space).
//! - The DSML sentinel is the 6 characters `｜DSML｜`.
//!
//! Inbound parsing tolerates model drift: a bare invoke without a wrapper
//! block, repeated closing tags, a missing `string=` attribute (inferred from
//! the value), `\r\n` line endings and surrounding whitespace.

use serde_json::Value;

use crate::dialect::{MalformedCall, ScanEvent, ToolCall, MAX_STRAY_CLOSERS};

/// BOS / EOS / role markers, copied from the V4 template.
pub const BOS: &str = "<｜begin▁of▁sentence｜>";
pub const EOS: &str = "<｜end▁of▁sentence｜>";
pub const USER_OPEN: &str = "<｜User｜>";
pub const ASSISTANT_OPEN: &str = "<｜Assistant｜>";

/// The DSML sentinel: exactly `｜DSML｜`.
pub const DSML: &str = "｜DSML｜";

/// The taught wire form: the doubled sentinel
/// `\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C}` with a short `calls` wrapper.
/// Inbound parsing folds it back to [`DSML`], so both spellings parse; this
/// one is what the prompt teaches and what history replay emits.
pub const DSML_TEACH: &str = "\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C}";
/// Thinking region markers from the V4 template. Built with `concat!` so the
/// source cannot be mangled by tooling that treats angle-bracket text as HTML.
pub const THINK_OPEN: &str = concat!("<", "think", ">");
pub const THINK_CLOSE: &str = concat!("</", "think", ">");

fn open(tag: &str) -> String {
    if tag.ends_with(' ') {
        format!("<{DSML}{tag}")
    } else {
        format!("<{DSML}{tag}>")
    }
}
fn close(tag: &str) -> String {
    format!("</{DSML}{tag}>")
}

/// Normalise line endings.
fn normalize(text: &str) -> String {
    // Fold the doubled sentinel we teach (and any drifted mixture of the two
    // spellings) back to the canonical single sentinel every parser path
    // below matches on. Longest-first, so a doubled tag never half-matches.
    let folded = text.replace(DSML_TEACH, DSML);
    folded.replace("\r\n", "\n").replace('\r', "\n")
}

// ---------------------------------------------------------------------------
// Outbound rendering
// ---------------------------------------------------------------------------

/// `## Tools` ... schema lines ... footer, exactly as the template composes it.
pub fn tools_block(tools: &[Value]) -> String {
    if tools.is_empty() {
        return String::new();
    }
    let mut schemas = String::new();
    for tool in tools {
        if tool.get("type").and_then(|t| t.as_str()) != Some("function") {
            continue;
        }
        let Some(func) = tool.get("function") else {
            continue;
        };
        schemas.push_str(&serde_json::to_string(func).unwrap_or_default());
        schemas.push('\n');
    }
    let mut out = String::new();
    out.push_str("## Tools\n\n");
    out.push_str(
        "You have access to a set of tools to help answer the user's question. You can invoke tools by writing a \"",
    );
    out.push_str(&open("tool_calls"));
    out.push_str("\" block like the following:\n\n");
    out.push_str(&open("tool_calls"));
    out.push('\n');
    out.push_str(&open("invoke name=\"$TOOL_NAME\""));
    out.push('\n');
    out.push_str(&open("parameter name=\"$PARAMETER_NAME\" string=\"true|false\""));
    out.push_str("$PARAMETER_VALUE");
    out.push_str(&close("parameter"));
    out.push('\n');
    out.push_str("...\n");
    out.push_str(&close("invoke"));
    out.push('\n');
    out.push_str(&open("invoke name=\"$TOOL_NAME2\""));
    out.push_str("\n...\n");
    out.push_str(&close("invoke"));
    out.push('\n');
    out.push_str(&close("tool_calls"));
    out.push_str("\n\n");
    out.push_str(
        "String parameters should be specified as is and set `string=\"true\"`. For all other types (numbers, booleans, arrays, objects), pass the value in JSON format and set `string=\"false\"`.\n\n",
    );
    out.push_str("If thinking_mode is enabled (triggered by ");
    out.push_str(THINK_OPEN);
    out.push_str("), you MUST output your complete reasoning inside ");
    out.push_str(THINK_OPEN);
    out.push_str("...");
    out.push_str(THINK_CLOSE);
    out.push_str(" BEFORE any tool calls or final response.\n\n");
    out.push_str("Otherwise, output directly after ");
    out.push_str(THINK_CLOSE);
    out.push_str(" with tool calls or final response.\n\n");
    out.push_str("### Available Tool Schemas\n\n");
    out.push_str(&schemas);
    out.push_str("\nYou MUST strictly follow the above defined tool name and parameter schemas to invoke tool calls.\n");
    // Teach the doubled sentinel and the short `calls` wrapper. Parsing
    // accepts both this and the single-sentinel `tool_calls` form.
    let out = out
        .replace(&open("tool_calls"), &open("calls"))
        .replace(&close("tool_calls"), &close("calls"));
    out.replace(DSML, DSML_TEACH)
}

/// One parameter line. Strings go raw (`string="true"`); everything else is
/// re-serialised as JSON (`string="false"`).
fn parameter_line(key: &str, value: &Value) -> String {
    match value {
        Value::String(s) => {
            format!("<{DSML}parameter name=\"{key}\" string=\"true\">{s}</{DSML}parameter>")
        }
        other => format!(
            "<{DSML}parameter name=\"{key}\" string=\"false\">{}</{DSML}parameter>",
            serde_json::to_string(other).unwrap_or_else(|_| "null".into())
        ),
    }
}

fn invoke_block(name: &str, args: Option<&Value>) -> String {
    let map = match args {
        Some(Value::Object(map)) => Some(map),
        _ => None,
    };
    let mut out = String::new();
    out.push_str(&open(&format!("invoke name=\"{name}\"")));
    out.push('\n');
    match map {
        Some(map) if !map.is_empty() => {
            for (key, value) in map {
                out.push_str(&parameter_line(key, value));
                out.push('\n');
            }
        }
        _ => {}
    }
    out.push_str(&close("invoke"));
    out
}

/// One `<｜DSML｜invoke>` block from a parsed call.
pub fn render_tool_call(call: &ToolCall) -> String {
    invoke_block(&call.name, call.json().as_ref())
}

fn calls_block(calls: &[ToolCall]) -> String {
    let mut out = String::new();
    out.push_str(&open("calls"));
    out.push('\n');
    for call in calls {
        out.push_str(&render_tool_call(call));
        out.push('\n');
    }
    out.push_str(&close("calls"));
    out
}

/// A tool result, folded into the current user turn per the V4 template.
pub fn render_tool_result(_name: &str, content: &str) -> String {
    format!("<tool_result>{content}</tool_result>")
}

/// History replay: the assistant-side tool-call block, including the leading
/// `\n\n`, or `None` when there are no usable calls.
pub fn history_tool_calls(calls: &[Value]) -> Option<String> {
    let mut parsed = Vec::new();
    for call in calls {
        let Some(func) = call.get("function") else {
            continue;
        };
        let Some(name) = func.get("name").and_then(|n| n.as_str()) else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        let arguments = match func.get("arguments") {
            Some(Value::String(s)) => s.clone(),
            Some(v) => v.to_string(),
            None => "{}".to_string(),
        };
        parsed.push(ToolCall {
            name: name.to_string(),
            arguments,
        });
    }
    if parsed.is_empty() {
        None
    } else {
        Some(format!(
            "\n\n{}",
            calls_block(&parsed).replace(DSML, DSML_TEACH)
        ))
    }
}

/// Per-turn re-anchoring reminder for the DSML dialect.
pub fn reminder() -> String {
    let mut out = String::new();
    out.push_str("Tool call format (DSML): a malformed or repeated block is dropped and never run.\n");
    out.push_str("Calls use exactly this form:\n");
    out.push_str(&open("tool_calls"));
    out.push('\n');
    out.push_str(&open("invoke name=\"tool_name\""));
    out.push('\n');
    out.push_str(&open("parameter name=\"param1\" string=\"true\""));
    out.push_str("value1");
    out.push_str(&close("parameter"));
    out.push('\n');
    out.push_str(&open("parameter name=\"count\" string=\"false\""));
    out.push('5');
    out.push_str(&close("parameter"));
    out.push('\n');
    out.push_str(&close("invoke"));
    out.push('\n');
    out.push_str(&close("tool_calls"));
    out.push('\n');
    out.push_str("1. Every parameter tag must carry its string attribute: string=\"true\" ");
    out.push_str("for a plain string value, string=\"false\" for a number, boolean, null, ");
    out.push_str("array or object written as JSON. A missing string= attribute makes the ");
    out.push_str("value's type a guess.\n");
    let out = out
        .replace(&open("tool_calls"), &open("calls"))
        .replace(&close("tool_calls"), &close("calls"));
    out.replace(DSML, DSML_TEACH)
}

/// Generation prompt tail: `<｜Assistant｜>` plus the think open/close token.
pub fn generation_tail(thinking: bool) -> String {
    let mut out = String::from(ASSISTANT_OPEN);
    if thinking {
        out.push_str(THINK_OPEN);
    } else {
        out.push_str(THINK_CLOSE);
    }
    out
}

/// Render every non-system message per the V4 template.
///
/// Consecutive user/developer/tool messages share one `<｜User｜>` turn, joined
/// with `\n\n`; an assistant message closes the run.
pub fn render_transcript(messages: &[crate::Msg]) -> String {
    use crate::content_text;

    let mut out = String::new();
    let mut in_user = false;
    for m in messages {
        match m.role.as_str() {
            "system" => {}
            "user" | "developer" => {
                if in_user {
                    out.push_str("\n\n");
                } else {
                    out.push_str(USER_OPEN);
                    in_user = true;
                }
                out.push_str(&content_text(&m.content));
            }
            "tool" => {
                if in_user {
                    out.push_str("\n\n");
                } else {
                    out.push_str(USER_OPEN);
                    in_user = true;
                }
                let name = m.tool_call_id.as_deref().unwrap_or("");
                out.push_str(&render_tool_result(name, &content_text(&m.content)));
            }
            "assistant" => {
                in_user = false;
                out.push_str(ASSISTANT_OPEN);
                out.push_str(THINK_CLOSE);
                out.push_str(&content_text(&m.content));
                if let Some(block) = history_tool_calls(&m.tool_calls) {
                    out.push_str(&block);
                }
                out.push_str(EOS);
            }
            other => {
                if in_user {
                    out.push_str("\n\n");
                } else {
                    out.push_str(USER_OPEN);
                    in_user = true;
                }
                out.push_str(other);
                out.push_str(": ");
                out.push_str(&content_text(&m.content));
            }
        }
    }
    out
}

/// Full prompt for a fresh thread: BOS, every system message, then the tools
/// teaching block when tools were offered, then the rendered transcript.
pub fn render_prompt(messages: &[crate::Msg], tools: &[Value]) -> String {
    let mut out = String::from(BOS);
    let systems: Vec<String> = messages
        .iter()
        .filter(|m| m.role == "system")
        .map(|m| crate::content_text(&m.content))
        .filter(|s| !s.is_empty())
        .collect();
    out.push_str(&systems.join("\n\n"));
    let block = tools_block(tools);
    if !block.is_empty() {
        if !systems.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&block);
    }
    out.push_str(&render_transcript(messages));
    append_continue_if_tool(&mut out, messages);
    out
}

/// Prompt for a continuation turn: only the messages since the last assistant.
pub fn render_delta(messages: &[crate::Msg]) -> String {
    let delta = crate::delta_messages(messages);
    let mut out = render_transcript(delta);
    append_continue_if_tool(&mut out, delta);
    out
}

fn append_continue_if_tool(out: &mut String, messages: &[crate::Msg]) {
    let last = messages
        .iter()
        .rev()
        .find(|m| m.role != "system" && m.role != "developer")
        .map(|m| m.role.as_str());
    if last == Some("tool") {
        out.push_str("\n\n");
        out.push_str(crate::CONTINUE_AFTER_TOOL);
    }
}

// ---------------------------------------------------------------------------
// Inbound parsing
// ---------------------------------------------------------------------------

fn is_dsml_tag(inner: &str) -> Option<&str> {
    inner.strip_prefix(DSML).map(|r| r.trim())
}

/// Attribute lookup supporting double and single quotes.
fn attr(attrs: &str, key: &str) -> Option<String> {
    let needle = format!("{key}=");
    let idx = attrs.find(&needle)?;
    let after = attrs[idx + needle.len()..].trim_start();
    let quote = after.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let inner = &after[1..];
    let end = inner.find(quote)?;
    Some(inner[..end].to_string())
}

/// A parameter value that failed to parse as JSON, kept verbatim so the repair
/// path can report `invalid_arguments_json`.
enum ArgValue {
    Json(Value),
    Raw(String),
}

/// Parameter bodies, in the order the model emitted them.
fn parse_parameters(content: &str) -> Vec<(String, ArgValue)> {
    let mut out = Vec::new();
    let mut rest = content;
    while let Some(start) = rest.find('<') {
        let tail = &rest[start..];
        let Some(gt) = tail.find('>') else {
            break;
        };
        let inner = &tail[1..gt];
        let Some(rest_tag) = is_dsml_tag(inner) else {
            rest = &tail[1..];
            continue;
        };
        let Some(rest_tag) = rest_tag.strip_prefix("parameter ") else {
            rest = &tail[gt + 1..];
            continue;
        };
        let Some(name) = attr(rest_tag, "name") else {
            rest = &tail[gt + 1..];
            continue;
        };
        let body_and_rest = &tail[gt + 1..];
        let close_tag = close("parameter");
        let Some(close_rel) = body_and_rest.find(&close_tag) else {
            out.push((name, classify_value(attr(rest_tag, "string"), body_and_rest)));
            break;
        };
        let body = body_and_rest[..close_rel].trim();
        out.push((name, classify_value(attr(rest_tag, "string"), body)));
        rest = &body_and_rest[close_rel + close_tag.len()..];
    }
    out
}

/// `string="true"` -> verbatim string; `string="false"` -> JSON (raw on
/// failure); a missing attribute is inferred from the value.
fn classify_value(string_attr: Option<String>, body: &str) -> ArgValue {
    let body = body.trim();
    match string_attr.as_deref() {
        Some("true") => ArgValue::Json(Value::String(body.to_string())),
        Some("false") => match serde_json::from_str::<Value>(body) {
            Ok(v) => ArgValue::Json(v),
            Err(_) => ArgValue::Raw(body.to_string()),
        },
        _ => {
            if looks_like_json(body) {
                match serde_json::from_str::<Value>(body) {
                    Ok(v) => ArgValue::Json(v),
                    Err(_) => ArgValue::Json(Value::String(body.to_string())),
                }
            } else {
                ArgValue::Json(Value::String(body.to_string()))
            }
        }
    }
}

fn looks_like_json(body: &str) -> bool {
    let t = body.trim_start();
    let first = match t.chars().next() {
        Some(c) => c,
        None => return false,
    };
    first == '{'
        || first == '['
        || first.is_ascii_digit()
        || first == '-'
        || t.starts_with("true")
        || t.starts_with("false")
        || t.starts_with("null")
}

/// Serialise the argument pairs. A raw value is embedded unquoted, producing
/// invalid JSON on purpose: validation then reports `invalid_arguments_json`.
fn args_json(params: &[(String, ArgValue)]) -> String {
    let mut out = String::from("{");
    for (i, (key, value)) in params.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&serde_json::to_string(key).unwrap_or_default());
        out.push_str(": ");
        match value {
            ArgValue::Json(v) => out.push_str(&serde_json::to_string(v).unwrap_or_default()),
            ArgValue::Raw(raw) => out.push_str(raw),
        }
    }
    out.push('}');
    out
}

/// Build a call from an invoke open tag's inner text and its body.
fn invoke_from(open_inner: &str, body: &str) -> Option<ToolCall> {
    let rest = is_dsml_tag(open_inner)?.strip_prefix("invoke")?;
    let name = attr(rest, "name")?;
    if name.is_empty() {
        return None;
    }
    let params = parse_parameters(body);
    Some(ToolCall {
        name,
        arguments: args_json(&params),
    })
}

/// One complete reply, walked once, producing prose and calls in order.
fn parse_events(text: &str) -> Vec<ScanEvent> {
    let normalized = normalize(text);
    let mut events: Vec<ScanEvent> = Vec::new();
    let mut prose = String::new();
    let mut i = 0usize;
    let mut stray_closers = 0usize;
    let open_wrapper = open("tool_calls");
    let close_wrapper = close("tool_calls");
    let close_invoke = close("invoke");
    let close_param = close("parameter");
    let wrapper_inner = format!("{DSML}tool_calls");
    let wrapper_close_inner = format!("/{DSML}tool_calls");
    // The taught short wrapper (`calls`). Both wrappers are structural noise
    // and must never surface as prose, whichever one the model emits.
    let calls_inner = format!("{DSML}calls");
    let calls_close_inner = format!("/{DSML}calls");

    while i < normalized.len() {
        let Some(rel) = normalized[i..].find('<') else {
            prose.push_str(&normalized[i..]);
            break;
        };
        let start = i + rel;
        prose.push_str(&normalized[i..start]);
        let Some(gt_rel) = normalized[start..].find('>') else {
            prose.push_str(&normalized[start..]);
            break;
        };
        let gt = start + gt_rel;
        let inner = &normalized[start + 1..gt];
        let after = &normalized[gt + 1..];

        if inner == wrapper_inner
            || inner == wrapper_close_inner
            || inner == calls_inner
            || inner == calls_close_inner
        {
            i = gt + 1;
            continue;
        }
        if inner.starts_with(&close_invoke[1..close_invoke.len() - 1]) {
            // A closing tag with no open block: swallow up to the limit, then
            // leave the rest as prose so a literal tag survives.
            if stray_closers < MAX_STRAY_CLOSERS {
                stray_closers += 1;
                i = gt + 1;
                continue;
            }
            prose.push_str(&normalized[start..=gt]);
            i = gt + 1;
            continue;
        }
        stray_closers = 0;
        if let Some(tag) = is_dsml_tag(inner) {
            if tag.starts_with("invoke") {
                match after.find(&close_invoke) {
                    Some(close_rel) => {
                        let body = &after[..close_rel];
                        flush_text(&mut prose, &mut events);
                        match invoke_from(inner, body) {
                            Some(call) => events.push(ScanEvent::Tool(call)),
                            None => events.push(ScanEvent::Malformed(MalformedCall::new(
                                attr(tag, "name"),
                                body.to_string(),
                                "missing_name_or_empty_body",
                            ))),
                        }
                        i = gt + 1 + close_rel + close_invoke.len();
                        continue;
                    }
                    None => {
                        flush_text(&mut prose, &mut events);
                        events.push(ScanEvent::Malformed(MalformedCall::new(
                            attr(tag, "name"),
                            after.to_string(),
                            "unterminated_block",
                        )));
                        i = normalized.len();
                        continue;
                    }
                }
            }
            if tag.starts_with("parameter") {
                // An orphan parameter block, outside any invoke.
                flush_text(&mut prose, &mut events);
                let end = after
                    .find(&close_param)
                    .map(|r| r + close_param.len())
                    .unwrap_or(after.len());
                events.push(ScanEvent::Malformed(MalformedCall::new(
                    None,
                    format!("{}>{}", inner, &after[..end]),
                    "orphan_parameter_block",
                )));
                i = gt + 1 + end;
                continue;
            }
            // Unknown DSML tag: structural noise, dropped.
            i = gt + 1;
            continue;
        }
        // Ordinary text/tag: keep it in prose.
        prose.push_str(&normalized[start..=gt]);
        i = gt + 1;
    }
    flush_text(&mut prose, &mut events);
    let _ = (open_wrapper, close_wrapper);
    events
}

fn flush_text(prose: &mut String, events: &mut Vec<ScanEvent>) {
    if prose.is_empty() {
        return;
    }
    let text = std::mem::take(prose);
    events.push(ScanEvent::Text(text));
}

/// Extract the DSML tool calls from model text, returning the prose with the
/// whole `｜DSML｜` blocks removed and the calls in order.
pub fn parse_tool_calls(text: &str) -> (String, Vec<ToolCall>) {
    let mut prose = String::new();
    let mut calls = Vec::new();
    for event in parse_events(text) {
        match event {
            ScanEvent::Text(t) => prose.push_str(&t),
            ScanEvent::Tool(call) => calls.push(call),
            ScanEvent::Malformed(_) => {}
        }
    }
    (prose.trim().to_string(), calls)
}

/// Ordered text / tool / malformed events for one complete reply.
pub fn scan(text: &str) -> Vec<ScanEvent> {
    parse_events(text)
}

/// Incremental filter over the DSML grammar, chunk by chunk.
///
/// Prose outside tool blocks is released as soon as it is safe; a trailing
/// partial tag is held so it is never emitted half-formed. A complete invoke
/// becomes a [`ScanEvent::Tool`]; an unterminated one at `finish` becomes
/// malformed, never prose.
pub struct DsmlStreamFilter {
    pending: String,
    /// Completed calls, in discovery order.
    pub calls: Vec<ToolCall>,
    /// Blocks that failed to parse, in discovery order.
    pub malformed: Vec<MalformedCall>,
}

impl Default for DsmlStreamFilter {
    fn default() -> Self {
        Self::new()
    }
}

impl DsmlStreamFilter {
    pub fn new() -> Self {
        Self {
            pending: String::new(),
            calls: Vec::new(),
            malformed: Vec::new(),
        }
    }

    pub fn feed(&mut self, text: &str) -> String {
        self.pending.push_str(&normalize(text));
        let mut out = String::new();
        loop {
            match self.next_step() {
                StreamStep::DrainText(n) => {
                    out.push_str(&self.pending[..n]);
                    self.pending.drain(..n);
                    break;
                }
                StreamStep::Hold => break,
                StreamStep::Consume {
                    text_len,
                    consumed,
                    event,
                } => {
                    if text_len > 0 {
                        out.push_str(&self.pending[..text_len]);
                    }
                    if let Some(event) = event {
                        match event {
                            ScanEvent::Text(t) => out.push_str(&t),
                            ScanEvent::Tool(c) => self.calls.push(c),
                            ScanEvent::Malformed(m) => self.malformed.push(m),
                        }
                    }
                    self.pending.drain(..consumed);
                }
            }
        }
        out
    }

    pub fn finish(&mut self) -> String {
        let rest = std::mem::take(&mut self.pending);
        if rest.is_empty() {
            return String::new();
        }
        let open_invoke = open("invoke ");
        let Some(start) = rest.find(&open_invoke) else {
            return rest;
        };
        let after = &rest[start + open_invoke.len()..];
        let tag_end = match after.find('>') {
            Some(n) => n,
            None => return rest,
        };
        let name = attr(&after[..tag_end], "name");
        let body = after[tag_end + 1..].to_string();
        let reason = if body.trim().is_empty() {
            "missing_name_and_empty_body"
        } else {
            "unterminated_block"
        };
        self.malformed.push(MalformedCall::new(name, body, reason));
        rest[..start].to_string()
    }

    fn next_step(&self) -> StreamStep {
        let open_wrapper = open("tool_calls");
        let open_calls = open("calls");
        let close_wrapper = close("tool_calls");
        let close_calls = close("calls");
        let open_invoke = open("invoke ");
        let close_invoke = close("invoke");
        let close_param = close("parameter");

        let found = [open_wrapper.as_str(), open_calls.as_str(), open_invoke.as_str()]
            .iter()
            .filter_map(|needle| self.pending.find(needle).map(|at| (at, *needle)))
            .min_by_key(|(at, _)| *at);

        let Some((at, needle)) = found else {
            // Swallow stray closers that follow a completed block.
            let stray = stray_closer_len(&self.pending, &close_invoke)
                + stray_closer_len(&self.pending, &close_param)
                + stray_closer_len(&self.pending, &close_wrapper);
            if stray > 0 {
                return StreamStep::Consume {
                    text_len: 0,
                    consumed: stray,
                    event: None,
                };
            }
            let hold = partial_dsml_suffix(&self.pending);
            let safe = self.pending.len() - hold;
            return if safe > 0 {
                StreamStep::DrainText(safe)
            } else {
                StreamStep::Hold
            };
        };

        if needle == open_wrapper.as_str() || needle == open_calls.as_str() {
            return StreamStep::Consume {
                text_len: at,
                consumed: at + needle.len(),
                event: None,
            };
        }

        let after_open = &self.pending[at + needle.len()..];
        let tag_end = match after_open.find('>') {
            Some(n) => n,
            None => {
                return if at > 0 {
                    StreamStep::DrainText(at)
                } else {
                    StreamStep::Hold
                };
            }
        };
        let body_and_rest = &after_open[tag_end + 1..];

        let Some(close_rel) = body_and_rest.find(&close_invoke) else {
            return if at > 0 {
                StreamStep::DrainText(at)
            } else {
                StreamStep::Hold
            };
        };
        let body = &body_and_rest[..close_rel];
        let inner = &after_open[..tag_end];
        let mut event = match invoke_from(&format!("{DSML}invoke {inner}"), body) {
            Some(call) => ScanEvent::Tool(call),
            None => ScanEvent::Malformed(MalformedCall::new(
                attr(inner, "name"),
                body.to_string(),
                "missing_name_or_empty_body",
            )),
        };
        // A wrapper close that follows the last invoke belongs to the block.
        let mut consumed = at + needle.len() + tag_end + 1 + close_rel + close_invoke.len();
        let mut stray = stray_closer_len(&self.pending[consumed..], &close_invoke);
        consumed += stray;
        stray = stray_closer_len(&self.pending[consumed..], &close_calls)
            + stray_closer_len(&self.pending[consumed..], &close_wrapper);
        consumed += stray;
        let _ = &mut event;
        StreamStep::Consume {
            text_len: at,
            consumed,
            event: Some(event),
        }
    }
}

fn stray_closer_len(rest: &str, closer: &str) -> usize {
    let mut len = 0;
    let mut s = rest;
    for _ in 0..MAX_STRAY_CLOSERS {
        let trimmed = s.trim_start();
        if let Some(next) = trimmed.strip_prefix(closer) {
            len += s.len() - trimmed.len() + closer.len();
            s = next;
        } else {
            break;
        }
    }
    len
}

/// Length of a trailing partial tag (from the last `<` with no `>` yet), capped
/// so ordinary prose with a stray `<` is not held forever.
fn partial_dsml_suffix(s: &str) -> usize {
    let tag_prefixes = [format!("<{DSML}"), format!("</{DSML}")];
    match s.rfind('<') {
        Some(i)
            if !s[i..].contains('>')
                && tag_prefixes.iter().any(|prefix| {
                    prefix.starts_with(&s[i..]) || s[i..].starts_with(prefix)
                }) =>
        {
            let len = s.len() - i;
            if len <= 256 { len } else { 0 }
        }
        _ => 0,
    }
}

enum StreamStep {
    DrainText(usize),
    Hold,
    Consume {
        text_len: usize,
        consumed: usize,
        event: Option<ScanEvent>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(name: &str, args: &str) -> ToolCall {
        ToolCall {
            name: name.into(),
            arguments: args.into(),
        }
    }

    #[test]
    fn round_trips_name_and_arguments() {
        let original = call("read", "{\"path\": \"/tmp/x\"}");
        let rendered = render_tool_call(&original);
        let (prose, parsed) = parse_tool_calls(&format!("Sure.\n{rendered}"));
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].name, "read");
        assert_eq!(parsed[0].json().unwrap(), json!({"path": "/tmp/x"}));
        assert!(prose.contains("Sure."));
        assert!(!prose.contains("DSML"));
    }

    #[test]
    fn strings_render_raw_and_others_render_json() {
        let c = call("read", "{\"path\": \"/a\", \"count\": 5, \"deep\": true}");
        let rendered = render_tool_call(&c);
        assert!(rendered.contains(&format!(
            "{}string=\"true\">/a{}",
            open("parameter name=\"path\" "),
            close("parameter")
        )));
        assert!(rendered.contains(&format!(
            "{}string=\"false\">5{}",
            open("parameter name=\"count\" "),
            close("parameter")
        )));
        assert!(rendered.contains(&format!(
            "{}string=\"false\">true{}",
            open("parameter name=\"deep\" "),
            close("parameter")
        )));
    }

    #[test]
    fn missing_string_attribute_is_inferred() {
        let text = format!(
            concat!(
                "{}name=\"t\">\n",
                "{}name=\"path\">/a/b{}\n",
                "{}name=\"n\">7{}\n",
                "{}"
            ),
            open("invoke "),
            open("parameter "),
            close("parameter"),
            open("parameter "),
            close("parameter"),
            close("invoke")
        );
        let calls = parse_tool_calls(&text).1;
        assert_eq!(calls.len(), 1);
        let args = calls[0].json().unwrap();
        assert_eq!(args["path"], "/a/b");
        assert_eq!(args["n"], 7);
    }

    #[test]
    fn two_invokes_parse_in_order() {
        let text = format!(
            concat!(
                "{}\n",
                "{}name=\"a\">\n",
                "{}name=\"x\" string=\"false\">1{}\n",
                "{}\n",
                "{}name=\"b\">\n",
                "{}name=\"y\" string=\"true\">z{}\n",
                "{}\n",
                "{}"
            ),
            open("tool_calls"),
            open("invoke "),
            open("parameter "),
            close("parameter"),
            close("invoke"),
            open("invoke "),
            open("parameter "),
            close("parameter"),
            close("invoke"),
            close("tool_calls")
        );
        let calls = parse_tool_calls(&text).1;
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "a");
        assert_eq!(calls[0].json().unwrap(), json!({"x": 1}));
        assert_eq!(calls[1].name, "b");
        assert_eq!(calls[1].json().unwrap(), json!({"y": "z"}));
    }

    #[test]
    fn bare_invoke_parses() {
        let text = format!(
            concat!(
                "{}name=\"bash\">\n",
                "{}name=\"command\" string=\"true\">ls{}\n",
                "{}"
            ),
            open("invoke "),
            open("parameter "),
            close("parameter"),
            close("invoke")
        );
        let calls = parse_tool_calls(&text).1;
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "bash");
        assert_eq!(calls[0].json().unwrap(), json!({"command": "ls"}));
    }

    #[test]
    fn block_is_removed_from_the_returned_prose() {
        let text = format!(
            "before\n{}\n{}name=\"t\">\n{}\n{}\nafter",
            open("tool_calls"),
            open("invoke "),
            close("invoke"),
            close("tool_calls")
        );
        let (prose, calls) = parse_tool_calls(&text);
        assert_eq!(calls.len(), 1);
        assert!(prose.contains("before"));
        assert!(prose.contains("after"));
        assert!(!prose.contains("DSML"));
    }

    #[test]
    fn malformed_json_survives_as_raw_text_for_the_repair_path() {
        let text = format!(
            "{}\n{}name=\"t\">\n{}name=\"n\" string=\"false\">not{{valid{}\n{}\n{}",
            open("tool_calls"),
            open("invoke "),
            open("parameter "),
            close("parameter"),
            close("invoke"),
            close("tool_calls")
        );
        let calls = parse_tool_calls(&text).1;
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "t");
        assert!(
            serde_json::from_str::<Value>(&calls[0].arguments).is_err(),
            "arguments must stay invalid: {:?}",
            calls[0].arguments
        );
        assert!(calls[0].arguments.contains("not{valid"));
    }

    #[test]
    fn doubled_closing_tags_and_crlf_are_tolerated() {
        let text = format!(
            "{}\r\n{}\r\n{}\r\n{}\r\n{}\r\n{}\r\n{}",
            open("tool_calls"),
            open("invoke name=\"t\""),
            close("invoke"),
            close("invoke"),
            close("invoke"),
            close("tool_calls"),
            close("tool_calls")
        );
        let calls = parse_tool_calls(&text).1;
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "t");
    }

    #[test]
    fn invoke_without_arguments_renders_a_bare_newline() {
        let rendered = render_tool_call(&call("t", "{}"));
        assert!(rendered.contains(&format!(
            "{}\n{}",
            open("invoke name=\"t\""),
            close("invoke")
        )));
    }

    #[test]
    fn reminder_restates_the_invoke_shape_and_the_string_attribute() {
        let r = reminder();
        assert!(r.contains(&open("invoke ").replace(DSML, DSML_TEACH)));
        assert!(r.contains(&open("parameter ").replace(DSML, DSML_TEACH)));
        assert!(r.contains("string="));
    }

    #[test]
    fn generation_tail_carries_the_assistant_marker_and_thinking_token() {
        let on = generation_tail(true);
        let off = generation_tail(false);
        assert!(on.starts_with(ASSISTANT_OPEN));
        assert!(on.ends_with(THINK_OPEN));
        assert!(off.starts_with(ASSISTANT_OPEN));
        assert!(off.ends_with(THINK_CLOSE));
        assert_ne!(on, off);
    }

    #[test]
    fn tools_block_embeds_one_compact_schema_line_per_tool() {
        let tools = vec![json!({
            "type": "function",
            "function": {"name": "read", "description": "read a file", "parameters": {}}
        })];
        let text = tools_block(&tools);
        assert!(text.starts_with("## Tools\n\n"));
        assert!(text.contains("string=\"true\"") && text.contains("string=\"false\""));
        assert!(text.contains("### Available Tool Schemas\n\n"));
        let schema = text.split("### Available Tool Schemas\n\n").nth(1).unwrap();
        assert_eq!(serde_json::from_str::<Value>(schema.lines().next().unwrap()).unwrap(), tools[0]["function"]);
        assert!(text.ends_with("\nYou MUST strictly follow the above defined tool name and parameter schemas to invoke tool calls.\n"));
    }

    #[test]
    fn tools_block_without_tools_is_empty() {
        assert_eq!(tools_block(&[]), "");
    }

    #[test]
    fn tool_result_folds_into_the_user_turn() {
        assert_eq!(
            render_tool_result("read", "file contents"),
            "<tool_result>file contents</tool_result>"
        );
    }

    #[test]
    fn last_user_and_tool_messages_share_one_user_turn() {
        use crate::Msg;
        let messages = vec![
            Msg {
                role: "system".into(),
                content: Some(json!("sys")),
                tool_calls: Vec::new(),
                tool_call_id: None,
                name: None,
            },
            Msg {
                role: "user".into(),
                content: Some(json!("first")),
                tool_calls: Vec::new(),
                tool_call_id: None,
                name: None,
            },
            Msg {
                role: "tool".into(),
                content: Some(json!("result")),
                tool_calls: Vec::new(),
                tool_call_id: Some("id1".into()),
                name: None,
            },
            Msg {
                role: "user".into(),
                content: Some(json!("second")),
                tool_calls: Vec::new(),
                tool_call_id: None,
                name: None,
            },
        ];
        let text = render_prompt(&messages, &[]);
        assert!(text.starts_with(BOS));
        assert!(text.contains(&format!(
            "{USER_OPEN}first\n\n<tool_result>result</tool_result>\n\nsecond"
        )));
        assert_eq!(text.matches(USER_OPEN).count(), 1);
    }

    #[test]
    fn assistant_history_renders_thinking_region_calls_and_eos() {
        use crate::Msg;
        let messages = vec![Msg {
            role: "assistant".into(),
            content: Some(json!("helping")),
            tool_calls: vec![json!({
                "id": "x",
                "type": "function",
                "function": {"name": "read", "arguments": "{\"path\":\"/a\"}"}
            })],
            tool_call_id: None,
            name: None,
        }];
        let text = render_transcript(&messages);
        assert!(text.starts_with(ASSISTANT_OPEN));
        assert!(text.contains(THINK_CLOSE));
        assert!(text.contains("helping"));
        assert!(text.contains(DSML_TEACH));
        assert!(text.contains(&open("calls").replace(DSML, DSML_TEACH)));
        assert!(text.ends_with(EOS));
    }

    #[test]
    fn stream_filter_splits_a_block_across_chunks() {
        let mut f = DsmlStreamFilter::new();
        let block = format!(
            "{}name=\"t\">\n{}name=\"a\" string=\"true\">v{}\n{}",
            open("invoke "),
            open("parameter "),
            close("parameter"),
            close("invoke")
        );
        let mut out = f.feed("prefix ");
        let split = block.char_indices().nth(5).unwrap().0;
        out.push_str(&f.feed(&block[..split]));
        out.push_str(&f.feed(&block[split..]));
        out.push_str(&f.finish());
        assert_eq!(out, "prefix ");
        assert_eq!(f.calls.len(), 1);
        assert_eq!(f.calls[0].json().unwrap(), json!({"a": "v"}));
    }

    #[test]
    fn configured_stream_filter_converts_the_reported_call_shape() {
        use crate::dialect::{Dialect, StreamToolFilter};

        let text = concat!(
            " <｜DSML｜tool_calls>\n",
            " <｜DSML｜invoke name=\"bash\">\n",
            " <｜DSML｜parameter name=\"command\" string=\"true\">echo hello</｜DSML｜parameter>\n",
            " <｜DSML｜parameter name=\"timeout\" string=\"false\">10</｜DSML｜parameter>\n",
            " </｜DSML｜invoke>\n",
            " </｜DSML｜tool_calls>"
        );
        let mut filter = StreamToolFilter::new(Dialect::DeepSeekV4Dsml);
        let mut prose = String::new();
        for (start, ch) in text.char_indices() {
            prose.push_str(&filter.feed(&text[start..start + ch.len_utf8()]));
        }
        prose.push_str(&filter.finish());

        assert!(prose.trim().is_empty(), "DSML leaked as prose: {prose:?}");
        assert_eq!(filter.calls.len(), 1);
        assert_eq!(filter.calls[0].name, "bash");
        assert_eq!(
            filter.calls[0].json().unwrap(),
            json!({"command": "echo hello", "timeout": 10})
        );
    }

    #[test]
    fn stream_filter_holds_partial_prose_tag() {
        let mut f = DsmlStreamFilter::new();
        assert_eq!(f.feed("text <"), "text ");
        assert_eq!(f.feed("b"), "<b");
        assert_eq!(f.finish(), "");
    }

    #[test]
    fn scan_reports_an_unterminated_invoke_as_malformed() {
        let text = format!("{}name=\"t\">", open("invoke "));
        let events = scan(&text);
        assert!(events
            .iter()
            .any(|e| matches!(e, ScanEvent::Malformed(m) if m.reason == "unterminated_block")));
    }

    #[test]
    fn doubled_sentinel_and_short_wrapper_are_accepted() {
        let text = format!(
            "<{t}calls>\n<{t}invoke name=\"shell\">\n<{t}parameter name=\"arguments\" string=\"false\">{{\"command\":\"ls\"}}</{t}parameter>\n</{t}invoke>\n</{t}calls>",
            t = DSML_TEACH
        );
        let (prose, calls) = parse_tool_calls(&text);
        assert!(prose.trim().is_empty(), "DSML leaked as prose: {prose:?}");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(
            calls[0].json().unwrap(),
            json!({"arguments": {"command": "ls"}})
        );
    }

    #[test]
    fn taught_prompt_uses_the_doubled_sentinel_and_short_wrapper() {
        let block = tools_block(&[json!({
            "type": "function",
            "function": {"name": "bash", "parameters": {}}
        })]);
        assert!(block.contains(DSML_TEACH), "block: {block}");
        assert!(block.contains(&open("calls").replace(DSML, DSML_TEACH)));
        assert!(!block.contains("tool_calls"), "block: {block}");
        let r = reminder();
        assert!(r.contains(DSML_TEACH), "reminder: {r}");
        assert!(!r.contains("tool_calls"), "reminder: {r}");
    }
}
