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
///
/// `arguments` keeps the **raw** JSON text the model emitted, exactly like
/// Python's `function.arguments` string. The recovery policy needs the raw
/// text for two reasons: a repair prompt must report `invalid_arguments_json`
/// for a body that is not valid JSON, and the echo-detection fingerprint
/// canonicalises the parsed value rather than the source bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    pub name: String,
    /// Raw JSON text between the `<invoke>` tags, untrimmed except for the
    /// surrounding whitespace the parser strips.
    pub arguments: String,
}

impl ToolCall {
    /// Parsed arguments, or `None` when the body is not valid JSON.
    pub fn json(&self) -> Option<serde_json::Value> {
        serde_json::from_str(&self.arguments).ok()
    }
}

/// Which wire dialect the proxy speaks to the backend.
///
/// Selected once at startup (`[proxy] dialect` or `DIALECT`) and threaded
/// through prompt assembly, streaming and repair. `JsonInvoke` is the original
/// behaviour and must stay byte-identical; `SimpleToolTag` is the one-marker
/// `<tool>{json}` format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    JsonInvoke,
    SimpleToolTag,
}

impl Default for Dialect {
    fn default() -> Self {
        Self::JsonInvoke
    }
}

impl Dialect {
    pub const JSON_INVOKE: &'static str = "json_invoke";
    pub const SIMPLE_TOOL_TAG: &'static str = "simple_tool_tag";

    /// Parse a configured dialect name. Unknown values are a hard error: a
    /// silent fallback would teach one format and parse another.
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim() {
            Self::JSON_INVOKE => Ok(Self::JsonInvoke),
            Self::SIMPLE_TOOL_TAG => Ok(Self::SimpleToolTag),
            other => Err(format!(
                "unknown dialect {other:?}; expected \"{}\" or \"{}\"",
                Self::JSON_INVOKE,
                Self::SIMPLE_TOOL_TAG
            )),
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::JsonInvoke => Self::JSON_INVOKE,
            Self::SimpleToolTag => Self::SIMPLE_TOOL_TAG,
        }
    }

    /// One tool call as the model reads it (history replay and repair).
    pub fn render_tool_call(&self, call: &ToolCall) -> String {
        match self {
            Self::JsonInvoke => format!(
                "{INVOKE_OPEN} name=\"{}\">{}</invoke>",
                call.name, call.arguments
            ),
            Self::SimpleToolTag => crate::dialect_simple::render_tool_call(call),
        }
    }

    /// Extract calls plus the prose with their blocks removed.
    #[allow(dead_code)] // public dialect surface; exercised by tests
    pub fn parse_tool_calls(&self, text: &str) -> (String, Vec<ToolCall>) {
        match self {
            Self::JsonInvoke => (strip_tool_calls(text), extract_tool_calls(text)),
            Self::SimpleToolTag => crate::dialect_simple::parse_tool_calls(text),
        }
    }

    /// Teaching text appended for the offered tools (`""` when none).
    pub fn system_extra(&self, tools: &[serde_json::Value]) -> String {
        match self {
            Self::JsonInvoke => inject_tool_descriptions("", tools),
            Self::SimpleToolTag => crate::dialect_simple::tools_block(tools),
        }
    }

    /// Per-turn re-anchoring reminder.
    pub fn reminder(&self) -> String {
        match self {
            Self::JsonInvoke => periodic_reminder(),
            Self::SimpleToolTag => crate::dialect_simple::reminder(),
        }
    }

    /// How one tool result is shown to the model.
    pub fn render_tool_result(&self, name: &str, content: &str) -> String {
        match self {
            Self::JsonInvoke => format!("Tool result (id={name}):\n{content}"),
            Self::SimpleToolTag => format!("<tool_result>{content}</tool_result>"),
        }
    }

    /// Concrete wire shape restated by repair tiers.
    pub fn wire_shape(&self) -> String {
        match self {
            Self::JsonInvoke => TEMPLATE.to_string(),
            Self::SimpleToolTag => crate::dialect_simple::render_tool_call(&ToolCall {
                name: "tool_name".into(),
                arguments: "{\"param1\":\"value1\",\"count\":5}".into(),
            }),
        }
    }

    /// Ordered text / tool / malformed events for one complete reply.
    pub fn scan(&self, text: &str) -> Vec<ScanEvent> {
        match self {
            Self::JsonInvoke => scan(text),
            Self::SimpleToolTag => crate::dialect_simple::scan(text),
        }
    }
}

/// `(name, canonical json)` — the port of Python's `fingerprint()`.
///
/// `serde_json::Map` is a `BTreeMap` (the workspace does not enable
/// `preserve_order`), so re-serialising a parsed value already sorts keys,
/// matching `json.dumps(..., sort_keys=True)`.
pub fn fingerprint(call: &ToolCall) -> (String, String) {
    let canonical = match call.json() {
        Some(v) => serde_json::to_string(&v).unwrap_or_else(|_| call.arguments.clone()),
        None => call.arguments.clone(),
    };
    (call.name.clone(), canonical)
}

pub const INVOKE_OPEN: &str = "<invoke";
pub const INVOKE_CLOSE: &str = "</invoke>";

/// How many stray closing tags right after a completed block are swallowed
/// instead of leaking into the prose. Models sometimes close twice or three
/// times; swallowing those keeps the reply clean, while anything past the
/// limit is left untouched so a literal tag in prose survives.
pub const MAX_STRAY_CLOSERS: usize = 2;

/// Drop up to [`MAX_STRAY_CLOSERS`] closing tags at the head of `s`.
fn strip_stray_closers(mut s: &str) -> &str {
    for _ in 0..MAX_STRAY_CLOSERS {
        match s.trim_start().strip_prefix(INVOKE_CLOSE) {
            Some(rest) => s = rest,
            None => break,
        }
    }
    s
}

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
    s.push_str("3. LITERAL MARKERS: never put a raw angle bracket or a raw closing\n");
    s.push_str("   marker inside a JSON string value; it ends the block early and the\n");
    s.push_str("   call is lost. If a value really needs one, write \\u003c.\n");
    s.push_str("4. MULTIPLE CALLS: you may emit several complete blocks back to back IN ONE\n");
    s.push_str("   TURN ONLY for short, independent, read-only calls (e.g. reading files,\n");
    s.push_str("   listing directories, grepping, small quick commands). NEVER batch writes,\n");
    s.push_str("   edits, long-running commands, or complex/stateful operations — emit exactly\n");
    s.push_str("   ONE such call per turn and wait for its result before the next.\n");
    s.push_str("5. You may write normal text before or after tool call blocks.\n");
    s.push_str("6. CALL ONLY WHEN NEEDED: if no tool fits, answer directly with no block.\n");
    s.push_str("   After a tool result arrives, continue the task — do not re-emit the\n");
    s.push_str("   same call.\n");
    s.push_str("7. NO GUESSING: use only parameters from the schema below; never invent values.");
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

/// Per-turn re-anchoring reminder, appended to the END of every turn's prompt.
///
/// The system prompt that teaches the format is only sent on the first turn of
/// a thread, so long threads drift and start emitting malformed blocks. This
/// restates the wire shape and the concrete failure modes (legacy marker tags,
/// a doubled closing tag) right before the model answers.
pub fn periodic_reminder() -> String {
    let mut s = String::new();
    s.push_str("Tool call format: a malformed or repeated block is dropped and never run.\n");
    s.push_str("Calls use exactly this form:\n");
    s.push_str(TEMPLATE);
    s.push_str("\n1. Do not emit the legacy marker spellings or related DSML tags;\n");
    s.push_str("   they reach the user and do nothing.\n");
    s.push_str("2. Emit the closing tag exactly once per tool call.\n");
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
#[allow(dead_code)]
pub fn extract_tool_calls(text: &str) -> Vec<ToolCall> {
    let normalized = normalize_dsml(text);
    let mut out = Vec::new();
    let mut rest = normalized.as_str();

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
            // A block is a call only when its body parses as a JSON object;
            // anything else is a malformed event, exactly like Python's
            // parser (which json.loads-es the body before emitting `tool`).
            if matches!(
                serde_json::from_str::<serde_json::Value>(body),
                Ok(serde_json::Value::Object(_))
            ) {
                out.push(ToolCall {
                    name,
                    arguments: body.to_string(),
                });
            }
        }

        rest = &body_and_rest[close_rel + INVOKE_CLOSE.len()..];
        rest = strip_stray_closers(rest);
    }

    out
}

/// Rewrite legacy DSML spellings of the invoke tag to the plain `<invoke>`
/// form, and drop DSML wrapper tags. Attributes are preserved on the invoke
/// tag; wrapper tags carry no data.
///
/// Models occasionally fall back to the old marker spelling (single or double
/// fullwidth pipes, optional spaces). Parsing stays universal on purpose, so an
/// old transcript or a drifting model still works no matter what we teach.
pub fn normalize_dsml(text: &str) -> String {
    if !text.contains("DSML") && !text.contains("dsml") {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while let Some(rel) = text[i..].find('<') {
        let start = i + rel;
        let Some(gt_rel) = text[start..].find('>') else {
            break;
        };
        let gt = start + gt_rel;
        let inner = &text[start + 1..gt];

        // Compact form: drop fullwidth pipes and whitespace so every marker
        // spelling collapses to the same shape (`dsmlinvokename=...`).
        let compact: String = inner
            .chars()
            .filter(|c| *c != '｜' && !c.is_whitespace())
            .collect::<String>()
            .to_ascii_lowercase();
        let rest = compact.strip_prefix("dsml");
        let rest = rest.or_else(|| compact.strip_prefix("/dsml"));
        let Some(rest) = rest else {
            out.push_str(&text[i..gt + 1]);
            i = gt + 1;
            continue;
        };

        out.push_str(&text[i..start]);
        let closing = compact.starts_with('/');
        if rest.starts_with("invoke") {
            if closing {
                out.push_str("</invoke>");
            } else {
                out.push_str("<invoke");
                if let Some(name) = attr_value(inner, "name") {
                    out.push_str(&format!(" name=\"{name}\""));
                }
                out.push('>');
            }
        }
        // Wrapper tags (`tool_calls`, `function_calls`, `calls`) are dropped.
        i = gt + 1;
    }
    out.push_str(&text[i..]);
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
#[allow(dead_code)] // kept for callers that render a single answer string
pub fn strip_tool_calls(text: &str) -> String {
    let normalized = normalize_dsml(text);
    let mut out = String::new();
    let mut rest = normalized.as_str();
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

/// Re-serialize OpenAI `tool_calls` into the wire format the model is taught.
///
/// History is flattened as text, so without this a replayed assistant turn
/// loses the arguments it actually sent.
#[allow(dead_code)] // compatibility wrapper for the original json dialect
pub fn format_tool_calls_for_history(calls: &[serde_json::Value]) -> String {
    format_tool_calls_for_history_with(calls, Dialect::JsonInvoke)
}

pub fn format_tool_calls_for_history_with(calls: &[serde_json::Value], wire: Dialect) -> String {
    let mut blocks = Vec::new();
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
        let body = match func.get("arguments") {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(v) => v.to_string(),
            None => "{}".to_string(),
        };
        // History replay must round-trip the exact bytes the model sent, so
        // the body is inserted verbatim rather than re-serialised.
        blocks.push(wire.render_tool_call(&ToolCall {
            name: name.to_string(),
            arguments: body,
        }));
    }
    blocks.join("\n")
}

/// One `<invoke>` block that could not be turned into a call.
///
/// Port of the records `extract_malformed()` yields in
/// `deepseek_proxy/tools/format.py`: `name` is `None` when the block never named
/// a tool (an orphan parameter block), and `reason` is a stable category, never
/// a dump of the body — a repair prompt must not replay the bad call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MalformedCall {
    pub name: Option<String>,
    pub body: String,
    pub reason: String,
}

impl MalformedCall {
    pub fn new(name: Option<String>, body: String, reason: &str) -> Self {
        Self {
            name,
            body,
            reason: reason.to_string(),
        }
    }
}

/// Reasonable ceiling on how much of a bad body is kept for diagnostics.
const MAX_MALFORMED_BODY: usize = 1500;

fn truncate_body(s: &str) -> String {
    let t = s.trim();
    if t.len() <= MAX_MALFORMED_BODY {
        t.to_string()
    } else {
        let mut end = MAX_MALFORMED_BODY;
        while !t.is_char_boundary(end) {
            end -= 1;
        }
        t[..end].to_string()
    }
}

/// Why a block failed, in the same categories the Python detector reports.
fn classify_body(name: Option<&str>, body: &str) -> Option<&'static str> {
    let body = body.trim();
    if body.is_empty() {
        return Some(if name.is_none() {
            "missing_name_and_empty_body"
        } else {
            "invalid_json_body"
        });
    }
    match serde_json::from_str::<serde_json::Value>(body) {
        Ok(serde_json::Value::Object(_)) => None,
        Ok(other) => {
            let _ = other;
            Some("body_not_object")
        }
        Err(_) => Some("invalid_json_body"),
    }
}

/// Records for `<invoke>` blocks that fail to parse, plus orphan parameter
/// blocks and an unterminated trailing block.
///
/// Kept as the tested reference detector; the live path uses [`scan`], which
/// reports the same failures in model-output order.
#[allow(dead_code)]
pub fn extract_malformed(text: &str) -> Vec<MalformedCall> {
    let normalized = normalize_dsml(text);
    let mut out: Vec<MalformedCall> = Vec::new();
    let mut seen: Vec<(Option<String>, String)> = Vec::new();
    let mut covered: Vec<(usize, usize)> = Vec::new();

    let mut push = |name: Option<String>, body: String, reason: &str, out: &mut Vec<_>| {
        let key = (name.clone(), body.clone());
        if seen.contains(&key) {
            return;
        }
        seen.push(key);
        out.push(MalformedCall::new(name, truncate_body(&body), reason));
    };

    let mut rest = normalized.as_str();
    let mut base = 0usize;
    while let Some(start) = rest.find(INVOKE_OPEN) {
        let after_open = &rest[start + INVOKE_OPEN.len()..];
        let Some(first) = after_open.chars().next() else {
            break;
        };
        if !first.is_whitespace() {
            base += start + INVOKE_OPEN.len();
            rest = after_open;
            continue;
        }
        let Some(tag_end) = after_open.find('>') else {
            break;
        };
        let attrs = &after_open[..tag_end];
        let name = attr_value(attrs, "name").filter(|n| !n.is_empty());
        let body_and_rest = &after_open[tag_end + 1..];

        let span_start = base + start;
        match body_and_rest.find(INVOKE_CLOSE) {
            Some(close_rel) => {
                let body = &body_and_rest[..close_rel];
                if let Some(reason) = classify_body(name.as_deref(), body) {
                    push(name, body.to_string(), reason, &mut out);
                }
                let consumed =
                    start + INVOKE_OPEN.len() + tag_end + 1 + close_rel + INVOKE_CLOSE.len();
                covered.push((span_start, base + consumed));
                base += consumed;
                rest = &rest[consumed..];
            }
            None => {
                // Output cut by the token limit before the closing tag.
                let body = body_and_rest;
                let reason = classify_body(name.as_deref(), body).unwrap_or("unterminated_block");
                push(name, body.to_string(), reason, &mut out);
                covered.push((span_start, base + rest.len()));
                break;
            }
        }
    }

    // A bare `<parameter name="x">value</parameter>` with no enclosing invoke:
    // the model confused the two levels. It must reach repair, not be treated
    // as ordinary prose.
    for (at, _) in normalized.match_indices("<parameter") {
        if covered.iter().any(|(s, e)| *s <= at && at < *e) {
            continue;
        }
        let Some(open_end) = normalized[at..].find('>') else {
            continue;
        };
        let attrs = &normalized[at..at + open_end];
        let body_start = at + open_end + 1;
        let body_end = normalized[body_start..]
            .find("</parameter>")
            .map(|r| body_start + r)
            .unwrap_or(normalized.len());
        let mut body = format!("{attrs}>");
        body.push_str(&normalized[body_start..body_end]);
        push(None, body, "orphan_parameter_block", &mut out);
    }

    out
}

/// Correction-turn prompt for malformed blocks, without replaying the bad turn.
///
/// Deliberately minimal: name the failures, restate the wire format, and tell
/// the model to follow the protocol it was already given. Per-call schemas for
/// argument failures are appended by [`crate::Recovery::begin_repair`] via
/// [`repair_schemas`]; the full catalogue is never sent.
/// How a repair turn is phrased. Escalates as attempts are spent.
///
/// The tiers deliberately get *simpler*. A model that has ignored a long,
/// jargon-heavy instruction twice is not helped by being told the same thing a
/// third time in the same words: the practical fix reported by operators is
/// "think before emitting, once" - short, plain, and specific. So tier 0 keeps
/// the original wording, tier 1 drops the protocol essay, and tier 2 is one
/// short sentence plus the wire shape.
fn repair_tier(attempt: usize) -> usize {
    // `attempt` is 1-based at the call site, so the first repair is tier 0.
    attempt.saturating_sub(1) % 3
}

/// Build the repair prompt for `attempt` (1-based).
///
/// `malformed` is the failure list for *this* attempt. It keeps the tool name
/// and reason across retries: an earlier version replaced everything with a
/// nameless `repair_omitted_call`, so attempts two and three told the model
/// nothing about what it had got wrong.
#[allow(dead_code)] // json wrapper; the live path selects a dialect
pub fn build_repair_prompt(
    malformed: &[MalformedCall],
    specs: &[ToolSpec],
    attempt: usize,
) -> String {
    build_repair_prompt_for(&Dialect::JsonInvoke, malformed, specs, attempt)
}

/// Dialect-aware repair prompt; [`build_repair_prompt`] is the json wrapper.
pub fn build_repair_prompt_for(
    dialect: &Dialect,
    malformed: &[MalformedCall],
    specs: &[ToolSpec],
    attempt: usize,
) -> String {
    let tier = repair_tier(attempt);
    let wire_shape = dialect.wire_shape();
    let mut out = String::new();

    match tier {
        0 => out.push_str(
            "Your previous response contained malformed tool call(s) that could not be executed. \
             Do not quote, copy, explain, or repeat it. Re-emit EACH corrected call now, \
             and nothing else.",
        ),
        1 => out.push_str(
            "That tool call could not be run, so nothing happened. The problem is only \
             the call's format, not what you were trying to do. Send the same call again, \
             correctly, and nothing else.",
        ),
        _ => out.push_str(
            "Still not usable. Send exactly one tool call, in the shape below, with no \
             other text.",
        ),
    }

    for (i, m) in malformed.iter().enumerate() {
        let name = match &m.name {
            Some(n) => format!("\"{n}\""),
            None => "None".to_string(),
        };
        // Strip any detail after the category separator, mirroring Python.
        let reason = m.reason.split(':').next().unwrap_or("malformed_tool_call");
        out.push_str(&format!(
            "\nFailure {}: name={} reason={}",
            i + 1,
            name,
            reason
        ));
    }
    if !specs.is_empty() {
        let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        out.push_str(&format!("\nValid tool names: {}.", names.join(", ")));
    }

    match tier {
        0 => {
            out.push_str(
                "\nThink about the format before you emit anything. Which tag opens the block, what \
                 goes between the tags, and which tag closes it? What does the JSON body look like, \
                 and does the tool name you are about to use appear in the valid list above? Work \
                 through that in your reasoning, then emit the call once.\n",
            );
            // The per-turn reminder carries the wire shape and the concrete failure
            // modes (legacy marker tags, doubled closing tag); a first repair turn
            // needs the same warning, not a weaker one.
            out.push_str("\n\n");
            out.push_str(&dialect.reminder());
            out.push_str("\n\nOnly fix the listed failures. Do not repeat calls already accepted.");
        }
        1 => {
            out.push_str("\n\nReminder of the wire shape:\n");
            out.push_str(&wire_shape);
        }
        _ => {
            out.push_str("\n\n");
            out.push_str(&wire_shape);
        }
    }

    out
}

/// Schemas worth re-sending on a repair turn, one per tool.
///
/// Deliberately narrow. A schema only helps when the model already named a real
/// tool and got its *arguments* wrong — then the property names and required
/// list are exactly what it is missing. For an unknown tool name, a missing
/// required call, or an omitted call there is nothing to key on, and dumping
/// the catalogue would only burn the context the repair needs.
///
/// The name comes from the model, so an unknown name matches no spec and adds
/// nothing. Same tool failing twice contributes one schema.
pub fn repair_schemas(malformed: &[MalformedCall], specs: &[ToolSpec]) -> Vec<serde_json::Value> {
    let mut seen: Vec<&str> = Vec::new();
    let mut out = Vec::new();
    for m in malformed {
        let Some(name) = m.name.as_deref() else {
            continue;
        };
        let category = m.reason.split(':').next().unwrap_or_default();
        // These are the reasons that actually reach a `MalformedCall`: the
        // wire-shape names come from `classify_body`, and the schema name comes
        // from `validation_reason`. (`invalid_arguments_json` and
        // `arguments_must_be_object` are internal to `validation_reason` and are
        // never recorded — matching on them here was a dead branch.)
        if !matches!(
            category,
            "arguments_do_not_match_schema" | "invalid_json_body" | "body_not_object"
        ) {
            continue;
        }
        let Some(spec) = specs.iter().find(|s| s.name == name) else {
            continue;
        };
        if !seen.contains(&name) {
            seen.push(name);
            out.push(spec.raw.clone());
        }
    }
    out
}

/// One tool offered to the model: the name used for lookup plus the raw
/// OpenAI tool object, which is what the repair prompt dumps verbatim.
#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub raw: serde_json::Value,
}

/// Tool specs in request order, skipping entries without a usable name.
pub fn tool_specs(tools: &[serde_json::Value]) -> Vec<ToolSpec> {
    let mut out = Vec::new();
    for tool in tools {
        let Some(func) = tool.get("function") else {
            continue;
        };
        let Some(name) = func.get("name").and_then(|n| n.as_str()) else {
            continue;
        };
        out.push(ToolSpec {
            name: name.to_string(),
            raw: tool.clone(),
        });
    }
    out
}

/// Validate one parsed call exactly like `Recovery.accept()` does.
///
/// Returns the stable failure category, or `None` when the call is acceptable.
/// The categories and their order of precedence match Python: an unknown tool
/// is reported before the arguments are even inspected.
pub fn validation_reason(specs: &[ToolSpec], call: &ToolCall) -> Option<&'static str> {
    let Some(spec) = specs.iter().find(|s| s.name == call.name) else {
        return Some("unknown_or_disallowed_tool");
    };
    let Ok(args) = serde_json::from_str::<serde_json::Value>(&call.arguments) else {
        return Some("invalid_arguments_json");
    };
    if !args.is_object() {
        return Some("arguments_must_be_object");
    }
    let parameters = spec
        .raw
        .get("function")
        .and_then(|f| f.get("parameters"))
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    match jsonschema::validator_for(&parameters) {
        Ok(validator) => {
            if validator.is_valid(&args) {
                None
            } else {
                Some("arguments_do_not_match_schema")
            }
        }
        // Python rejects an invalid schema at request-validation time; if one
        // slips through here, treat it as permissive rather than failing the turn.
        Err(_) => None,
    }
}

/// One parsed piece of model output, in the order the model produced it.
///
/// This is the flattened equivalent of the events `ToolParser` yields: text,
/// a valid call, or a block that failed to parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanEvent {
    Text(String),
    Tool(ToolCall),
    Malformed(MalformedCall),
}

/// Split model text into ordered text / tool / malformed events.
///
/// Port of feeding the whole reply through `ToolParser`: unlike
/// `extract_tool_calls` + `extract_malformed` run separately, this preserves the
/// interleaving so the recovery loop sees failures in the order they occurred.
pub fn scan(text: &str) -> Vec<ScanEvent> {
    let normalized = normalize_dsml(text);
    let mut out: Vec<ScanEvent> = Vec::new();
    let mut buf = String::new();
    let mut rest = normalized.as_str();

    while let Some(start) = rest.find(INVOKE_OPEN) {
        let after_open = &rest[start + INVOKE_OPEN.len()..];
        let Some(first) = after_open.chars().next() else {
            break;
        };
        if !first.is_whitespace() {
            buf.push_str(&rest[..start + INVOKE_OPEN.len()]);
            rest = after_open;
            continue;
        }
        let Some(tag_end) = after_open.find('>') else {
            break;
        };
        let attrs = &after_open[..tag_end];
        let body_and_rest = &after_open[tag_end + 1..];
        let name = attr_value(attrs, "name").filter(|n| !n.is_empty());

        match body_and_rest.find(INVOKE_CLOSE) {
            Some(close_rel) => {
                let body = &body_and_rest[..close_rel];
                buf.push_str(&rest[..start]);
                if !buf.is_empty() {
                    out.push(ScanEvent::Text(std::mem::take(&mut buf)));
                }
                match classify_body(name.as_deref(), body) {
                    None => {
                        if let Some(name) = name {
                            out.push(ScanEvent::Tool(ToolCall {
                                name,
                                arguments: body.trim().to_string(),
                            }));
                        }
                    }
                    Some(reason) => out.push(ScanEvent::Malformed(MalformedCall::new(
                        name,
                        body.to_string(),
                        reason,
                    ))),
                }
                rest = &body_and_rest[close_rel + INVOKE_CLOSE.len()..];
                rest = strip_stray_closers(rest);
            }
            None => {
                // Output cut before the closing tag: an unterminated block.
                buf.push_str(&rest[..start]);
                if !buf.is_empty() {
                    out.push(ScanEvent::Text(std::mem::take(&mut buf)));
                }
                let reason =
                    classify_body(name.as_deref(), body_and_rest).unwrap_or("unterminated_block");
                out.push(ScanEvent::Malformed(MalformedCall::new(
                    name,
                    body_and_rest.to_string(),
                    reason,
                )));
                rest = "";
            }
        }
    }
    buf.push_str(rest);
    if !buf.is_empty() {
        out.push(ScanEvent::Text(buf));
    }

    // A bare `<parameter name="x">value</parameter>` with no enclosing invoke:
    // the model confused the two levels, so it must reach repair, not prose.
    let mut with_orphans = Vec::with_capacity(out.len());
    for event in out {
        match event {
            ScanEvent::Text(text) if text.contains("<parameter") => {
                let mut remainder = text.as_str();
                while let Some(at) = remainder.find("<parameter") {
                    if at > 0 {
                        with_orphans.push(ScanEvent::Text(remainder[..at].to_string()));
                    }
                    let tail = &remainder[at..];
                    let open_end = tail.find('>').map(|i| i + 1).unwrap_or(tail.len());
                    let body_end = tail
                        .find("</parameter>")
                        .map(|i| i + "</parameter>".len())
                        .unwrap_or(tail.len());
                    let block = &tail[..body_end.max(open_end)];
                    with_orphans.push(ScanEvent::Malformed(MalformedCall::new(
                        None,
                        block.to_string(),
                        "orphan_parameter_block",
                    )));
                    remainder = &tail[body_end.max(open_end)..];
                }
                if !remainder.is_empty() {
                    with_orphans.push(ScanEvent::Text(remainder.to_string()));
                }
            }
            other => with_orphans.push(other),
        }
    }
    with_orphans
}

/// Tool names offered to the model, in request order, deduplicated.
///
/// Superseded by [`tool_specs`] on the live path; kept for the ordering test.
#[allow(dead_code)]
pub fn tool_names(tools: &[serde_json::Value]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for tool in tools {
        let Some(name) = tool
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(|n| n.as_str())
        else {
            continue;
        };
        if !out.iter().any(|n| n == name) {
            out.push(name.to_string());
        }
    }
    out
}

/// Python's `json.dumps(value, ensure_ascii=False)` with default separators.
///
/// Used for the `Available schemas:` dump so the repair prompt matches Python's
/// byte for byte: `{"a": 1}` rather than serde's compact `{"a":1}`. Non-ASCII
/// is kept as-is, which is what `ensure_ascii=False` does.
pub fn py_json_dumps(value: &serde_json::Value) -> String {
    let mut out = String::new();
    write_py_json(value, &mut out);
    out
}

fn write_py_json(value: &serde_json::Value, out: &mut String) {
    match value {
        serde_json::Value::Null => out.push_str("null"),
        serde_json::Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        serde_json::Value::Number(n) => out.push_str(&n.to_string()),
        serde_json::Value::String(s) => {
            out.push('"');
            for ch in s.chars() {
                match ch {
                    '"' => out.push_str("\\\""),
                    '\\' => out.push_str("\\\\"),
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\t' => out.push_str("\\t"),
                    '\u{08}' => out.push_str("\\b"),
                    '\u{0c}' => out.push_str("\\f"),
                    c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                    c => out.push(c),
                }
            }
            out.push('"');
        }
        serde_json::Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_py_json(item, out);
            }
            out.push(']');
        }
        serde_json::Value::Object(map) => {
            out.push('{');
            for (i, (key, val)) in map.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_py_json(&serde_json::Value::String(key.clone()), out);
                out.push_str(": ");
                write_py_json(val, out);
            }
            out.push('}');
        }
    }
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
        assert_eq!(calls[0].json().unwrap(), json!({"command": "ls"}));
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
        assert_eq!(calls[1].json().unwrap(), json!({"y": [1, 2]}));
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
    fn normalizes_dsml_invoke_tags() {
        // Single-pipe DSML spelling, with attributes preserved.
        let text = "<｜DSML｜invoke name=\"bash\">{\"command\":\"ls\"}</｜DSML｜invoke>";
        let calls = extract_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "bash");
        assert_eq!(calls[0].json().unwrap(), json!({"command": "ls"}));
    }

    #[test]
    fn normalizes_dsml_with_wrapper_and_double_pipes() {
        let text = "<｜｜DSML｜｜tool_calls><｜｜DSML｜｜invoke name=\"a\">{\"x\":1}</｜｜DSML｜｜invoke></｜｜DSML｜｜tool_calls>";
        let calls = extract_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "a");
        assert_eq!(calls[0].json().unwrap(), json!({"x": 1}));
    }

    #[test]
    fn strip_removes_dsml_blocks_too() {
        let text = "before <｜DSML｜invoke name=\"t\">{\"a\":1}</｜DSML｜invoke> after";
        let stripped = strip_tool_calls(text);
        assert!(stripped.contains("before"));
        assert!(stripped.contains("after"));
        assert!(!stripped.contains("DSML"));
    }

    #[test]
    fn detector_reports_invalid_json() {
        let found = extract_malformed("<invoke name=\"bash\">not json</invoke>");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name.as_deref(), Some("bash"));
        assert_eq!(found[0].reason, "invalid_json_body");
    }

    #[test]
    fn detector_reports_non_object_body() {
        let found = extract_malformed("<invoke name=\"a\">[1,2]</invoke>");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].reason, "body_not_object");
    }

    #[test]
    fn detector_ignores_good_blocks() {
        assert!(extract_malformed("<invoke name=\"a\">{\"x\":1}</invoke>").is_empty());
    }

    #[test]
    fn detector_finds_unterminated_block() {
        let found = extract_malformed("text <invoke name=\"a\">{\"x\":1}");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name.as_deref(), Some("a"));
        assert_eq!(found[0].reason, "unterminated_block");
    }

    #[test]
    fn detector_finds_orphan_parameter_block() {
        let found = extract_malformed("<parameter name=\"command\">ls</parameter>");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, None);
        assert_eq!(found[0].reason, "orphan_parameter_block");
    }

    #[test]
    fn detector_dedupes_repeated_blocks() {
        let block = "<invoke name=\"a\">bad</invoke>";
        let text = format!("{block}{block}");
        assert_eq!(extract_malformed(&text).len(), 1);
    }

    #[test]
    fn detector_handles_dsml_spelling() {
        let found = extract_malformed("<｜DSML｜invoke name=\"a\">nope</｜DSML｜invoke>");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].reason, "invalid_json_body");
    }

    #[test]
    fn repair_prompt_names_failures_without_bodies() {
        let malformed = vec![MalformedCall {
            name: Some("bash".into()),
            body: "not json".into(),
            reason: "invalid_json_body".into(),
        }];
        let specs = vec![
            ToolSpec {
                name: "bash".into(),
                raw: json!({"type": "function", "function": {"name": "bash", "parameters": {"type": "object"}}}),
            },
            ToolSpec {
                name: "edit".into(),
                raw: json!({"type": "function", "function": {"name": "edit", "parameters": {"type": "object"}}}),
            },
        ];
        let prompt = build_repair_prompt(&malformed, &specs, 1);
        assert!(prompt.contains("Failure 1: name=\"bash\" reason=invalid_json_body"));
        assert!(prompt.contains("Valid tool names: bash, edit."));
        assert!(prompt.contains(TEMPLATE));
        // The repair turn carries the same hard-format warning as a normal turn.
        // The first repair turn carries the per-turn reminder, which is now
        // plain rather than shouting; this marker is unique to it.
        assert!(prompt.contains("dropped and never run"));
        // `Recovery.begin_repair()` appends this line, so a repair turn must too.
        assert!(
            prompt.contains("Only fix the listed failures. Do not repeat calls already accepted.")
        );
        // The schemas are appended by `begin_repair`, not here.
        assert!(!prompt.contains("Available schemas"));
        // The bad body must not be replayed into the same conversation.
        assert!(!prompt.contains("not json"));
    }

    /// Each attempt gets a different, *simpler* opening: the same paragraph
    /// repeated verbatim is what the ladder exists to avoid, and the tier that
    /// drops the protocol essay must actually be shorter than tier 0.
    #[test]
    fn repair_prompt_escalates_and_gets_shorter() {
        let malformed = vec![MalformedCall {
            name: Some("bash".into()),
            body: "not json".into(),
            reason: "invalid_json_body".into(),
        }];
        let specs = vec![ToolSpec {
            name: "bash".into(),
            raw: json!({"type": "function", "function": {"name": "bash", "parameters": {"type": "object"}}}),
        }];

        let tiers: Vec<String> = (1..=3)
            .map(|attempt| build_repair_prompt(&malformed, &specs, attempt))
            .collect();

        // All three differ from one another.
        assert_ne!(tiers[0], tiers[1]);
        assert_ne!(tiers[1], tiers[2]);
        assert_ne!(tiers[0], tiers[2]);

        // Every tier still names the failure: the detail is what lets a model
        // fix the right thing, and it used to be lost after the first attempt.
        for (i, prompt) in tiers.iter().enumerate() {
            assert!(
                prompt.contains("Failure 1: name=\"bash\" reason=invalid_json_body"),
                "tier {i} lost the failure detail: {prompt}"
            );
            assert!(prompt.contains(TEMPLATE), "tier {i} lost the wire shape");
        }

        // The jargon goes away as attempts are spent, and never comes back.
        assert!(tiers[0].contains("dropped and never run"));
        assert!(!tiers[1].contains("dropped and never run"));
        assert!(!tiers[2].contains("dropped and never run"));
        assert!(tiers[1].len() < tiers[0].len());
        assert!(tiers[2].len() < tiers[1].len());
    }

    /// The tier rotates, so a fourth attempt is a second pass rather than a
    /// prompt the model has already ignored three times.
    #[test]
    fn repair_tier_round_robins() {
        assert_eq!(repair_tier(1), 0);
        assert_eq!(repair_tier(2), 1);
        assert_eq!(repair_tier(3), 2);
        assert_eq!(repair_tier(4), 0);
        // 0 saturates to 0, so it lands on tier 0 rather than underflowing.
        assert_eq!(repair_tier(0), 0, "0 must not panic or underflow");
    }

    #[test]
    fn tool_names_are_deduped_in_order() {
        let tools = vec![
            json!({"type": "function", "function": {"name": "b"}}),
            json!({"type": "function", "function": {"name": "a"}}),
            json!({"type": "function", "function": {"name": "b"}}),
        ];
        assert_eq!(tool_names(&tools), vec!["b", "a"]);
    }

    fn bash_spec() -> ToolSpec {
        ToolSpec {
            name: "bash".into(),
            raw: json!({
                "type": "function",
                "function": {
                    "name": "bash",
                    "parameters": {
                        "type": "object",
                        "properties": {"command": {"type": "string"}},
                        "required": ["command"]
                    }
                }
            }),
        }
    }

    fn call(name: &str, args: &str) -> ToolCall {
        ToolCall {
            name: name.into(),
            arguments: args.into(),
        }
    }

    #[test]
    fn validation_accepts_a_schema_conforming_call() {
        let specs = vec![bash_spec()];
        assert_eq!(
            validation_reason(&specs, &call("bash", "{\"command\": \"ls\"}")),
            None
        );
    }

    #[test]
    fn validation_rejects_unknown_tool() {
        let specs = vec![bash_spec()];
        assert_eq!(
            validation_reason(&specs, &call("nope", "{}")),
            Some("unknown_or_disallowed_tool")
        );
    }

    #[test]
    fn validation_rejects_non_object_arguments() {
        let specs = vec![bash_spec()];
        assert_eq!(
            validation_reason(&specs, &call("bash", "[1,2]")),
            Some("arguments_must_be_object")
        );
    }

    #[test]
    fn validation_rejects_invalid_json() {
        let specs = vec![bash_spec()];
        assert_eq!(
            validation_reason(&specs, &call("bash", "not json")),
            Some("invalid_arguments_json")
        );
    }

    #[test]
    fn validation_rejects_schema_mismatch() {
        let specs = vec![bash_spec()];
        // `command` is required by the schema and missing here.
        assert_eq!(
            validation_reason(&specs, &call("bash", "{}")),
            Some("arguments_do_not_match_schema")
        );
    }

    #[test]
    fn fingerprint_sorts_keys_like_python() {
        // Python canonicalises with sort_keys=True, so key order must not
        // distinguish two otherwise identical calls.
        let a = call("bash", "{\"b\": 1, \"a\": 2}");
        let b = call("bash", "{\"a\": 2, \"b\": 1}");
        assert_eq!(fingerprint(&a), fingerprint(&b));
    }

    #[test]
    fn scan_preserves_order_of_text_calls_and_failures() {
        let text = concat!(
            "before ",
            "<invoke name=\"bash\">{\"command\": \"ls\"}</invoke>",
            " middle ",
            "<invoke name=\"bash\">oops</invoke>",
            " after"
        );
        let events = scan(text);
        assert!(matches!(events[0], ScanEvent::Text(ref t) if t.contains("before")));
        assert!(matches!(events[1], ScanEvent::Tool(_)));
        assert!(matches!(events[2], ScanEvent::Text(ref t) if t.contains("middle")));
        assert!(matches!(events[3], ScanEvent::Malformed(_)));
        assert!(matches!(events[4], ScanEvent::Text(ref t) if t.contains("after")));
    }

    #[test]
    fn scan_reports_orphan_parameter_blocks() {
        let events = scan("text <parameter name=\"command\">ls</parameter>");
        assert!(events
            .iter()
            .any(|e| matches!(e, ScanEvent::Malformed(m) if m.reason == "orphan_parameter_block")));
    }

    #[test]
    fn py_json_dumps_matches_python_separators() {
        // Python: json.dumps({"a": 1, "b": [1, 2]}) == '{"a": 1, "b": [1, 2]}'
        let v = json!({"a": 1, "b": [1, 2]});
        assert_eq!(py_json_dumps(&v), "{\"a\": 1, \"b\": [1, 2]}");
    }

    #[test]
    fn py_json_dumps_keeps_non_ascii() {
        // ensure_ascii=False
        assert_eq!(py_json_dumps(&json!("héllo")), "\"héllo\"");
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
    inner: FilterInner,
    /// Completed calls, in discovery order.
    pub calls: Vec<ToolCall>,
    /// Blocks that failed to parse, in discovery order.
    pub malformed: Vec<MalformedCall>,
}

/// Dialect-specific streaming grammar.
enum FilterInner {
    Json(Parser),
    Simple(crate::dialect_simple::StreamFilter),
}

impl Default for StreamToolFilter {
    fn default() -> Self {
        Self::new(Dialect::JsonInvoke)
    }
}

impl StreamToolFilter {
    pub fn new(dialect: Dialect) -> Self {
        let inner = match dialect {
            Dialect::JsonInvoke => FilterInner::Json(Parser::new()),
            Dialect::SimpleToolTag => {
                FilterInner::Simple(crate::dialect_simple::StreamFilter::new())
            }
        };
        Self {
            inner,
            calls: Vec::new(),
            malformed: Vec::new(),
        }
    }

    /// Feed a text chunk; returns prose that is safe to emit as content now.
    pub fn feed(&mut self, text: &str) -> String {
        match &mut self.inner {
            FilterInner::Json(parser) => {
                let events = parser.feed(text);
                self.collect(events)
            }
            FilterInner::Simple(filter) => {
                let prose = filter.feed(text);
                self.calls = filter.calls.clone();
                self.malformed = filter.malformed.clone();
                prose
            }
        }
    }

    /// Flush at end of stream. A trailing partial tag is prose; an unterminated
    /// block is malformed, so it reaches repair instead of the client.
    pub fn finish(&mut self) -> String {
        match &mut self.inner {
            FilterInner::Json(parser) => {
                let events = parser.finish();
                self.collect(events)
            }
            FilterInner::Simple(filter) => {
                let prose = filter.finish();
                self.calls = filter.calls.clone();
                self.malformed = filter.malformed.clone();
                prose
            }
        }
    }

    fn collect(&mut self, events: Vec<ScanEvent>) -> String {
        let mut prose = String::new();
        for event in events {
            match event {
                ScanEvent::Text(text) => prose.push_str(&text),
                ScanEvent::Tool(call) => self.calls.push(call),
                ScanEvent::Malformed(bad) => self.malformed.push(bad),
            }
        }
        prose
    }
}

/// One step of the grammar, computed without mutating the buffer so the borrow
/// of `pending` ends before anything is drained.
enum Step {
    /// Emit `pending[..n]` as text, drop it, keep scanning.
    DrainText(usize),
    /// Emit `pending[..n]` as text and stop: the rest is incomplete.
    Hold(usize),
    /// Emit `pending[..text_len]` as text, emit `event`, drop the block.
    Consume {
        text_len: usize,
        consumed: usize,
        event: Option<ScanEvent>,
    },
}

/// Incremental driver over the same grammar [`scan`] implements whole.
///
/// `scan` stays the reference for a complete reply; this is that grammar driven
/// chunk by chunk, and the two agree on every case — including the one they
/// used to disagree on. An unterminated block is **malformed**, not prose: it
/// is a call that failed, so it routes to repair rather than to the client.
pub struct Parser {
    pending: String,
}

impl Default for Parser {
    fn default() -> Self {
        Self::new()
    }
}

impl Parser {
    pub fn new() -> Self {
        Self {
            pending: String::new(),
        }
    }

    /// Feed one chunk; returns every event that is now final.
    pub fn feed(&mut self, text: &str) -> Vec<ScanEvent> {
        self.pending.push_str(text);
        let normalized = normalize_dsml(&self.pending);
        if normalized != self.pending {
            self.pending = normalized;
        }
        let mut out = Vec::new();
        loop {
            match step(&self.pending) {
                Step::Hold(n) => {
                    if n > 0 {
                        out.push(ScanEvent::Text(self.pending[..n].to_string()));
                        self.pending.drain(..n);
                    }
                    break;
                }
                Step::DrainText(n) => {
                    out.push(ScanEvent::Text(self.pending[..n].to_string()));
                    self.pending.drain(..n);
                }
                Step::Consume {
                    text_len,
                    consumed,
                    event,
                } => {
                    if text_len > 0 {
                        out.push(ScanEvent::Text(self.pending[..text_len].to_string()));
                    }
                    out.extend(event);
                    self.pending.drain(..consumed);
                    // Doubled/tripled closing tags right after a block are
                    // swallowed, up to the limit.
                    let drop = self.pending.len() - strip_stray_closers(&self.pending).len();
                    self.pending.drain(..drop);
                }
            }
        }
        out
    }

    /// End of input. A trailing partial tag is prose; an unterminated block is
    /// malformed, matching [`scan`]'s rule for the same bytes.
    pub fn finish(&mut self) -> Vec<ScanEvent> {
        let rest = std::mem::take(&mut self.pending);
        if rest.is_empty() {
            return Vec::new();
        }
        let Some(start) = rest.find(INVOKE_OPEN) else {
            return vec![ScanEvent::Text(rest)];
        };
        let after = &rest[start + INVOKE_OPEN.len()..];
        let not_a_call = match after.chars().next() {
            None => true,
            Some(first) if !first.is_whitespace() => true,
            Some(_) => !after.contains('>'),
        };
        if not_a_call {
            return vec![ScanEvent::Text(rest)];
        }
        let tag_end = after.find('>').expect("checked just above");
        let name = attr_value(&after[..tag_end], "name").filter(|n| !n.is_empty());
        let body = &after[tag_end + 1..];
        let reason = classify_body(name.as_deref(), body).unwrap_or("unterminated_block");
        let mut out = Vec::new();
        if start > 0 {
            out.push(ScanEvent::Text(rest[..start].to_string()));
        }
        out.push(ScanEvent::Malformed(MalformedCall::new(
            name,
            body.to_string(),
            reason,
        )));
        out
    }
}

/// Classify the head of `pending` without mutating it.
fn step(pending: &str) -> Step {
    let Some(start) = pending.find(INVOKE_OPEN) else {
        return Step::Hold(pending.len() - partial_tag_suffix(pending));
    };
    let after = &pending[start + INVOKE_OPEN.len()..];
    let Some(first) = after.chars().next() else {
        return Step::Hold(start);
    };
    if !first.is_whitespace() {
        // `<invokex` is prose, not an opening tag.
        return Step::DrainText(start + INVOKE_OPEN.len());
    }
    let Some(tag_end) = after.find('>') else {
        return Step::Hold(start);
    };
    let body_and_rest = &after[tag_end + 1..];
    let Some(close_rel) = body_and_rest.find(INVOKE_CLOSE) else {
        return Step::Hold(start);
    };
    let name = attr_value(&after[..tag_end], "name").filter(|n| !n.is_empty());
    let body = &body_and_rest[..close_rel];
    let event = match classify_body(name.as_deref(), body) {
        None => name.map(|name| {
            ScanEvent::Tool(ToolCall {
                name,
                arguments: body.trim().to_string(),
            })
        }),
        Some(reason) => Some(ScanEvent::Malformed(MalformedCall::new(
            name,
            body.to_string(),
            reason,
        ))),
    };
    Step::Consume {
        text_len: start,
        consumed: start + INVOKE_OPEN.len() + tag_end + 1 + close_rel + INVOKE_CLOSE.len(),
        event,
    }
}

/// Length of the trailing incomplete tag in `s`: everything from the last
/// `<` that has no closing `>` yet. Capped so ordinary prose with a stray `<`
/// is not held back forever.
fn partial_tag_suffix(s: &str) -> usize {
    match s.rfind('<') {
        Some(i) if !s[i..].contains('>') => {
            let len = s.len() - i;
            if len <= 256 {
                len
            } else {
                0
            }
        }
        _ => 0,
    }
}

#[cfg(test)]
mod stream_tests {
    use super::*;

    #[test]
    fn passes_plain_text_through() {
        let mut f = StreamToolFilter::new(Dialect::JsonInvoke);
        assert_eq!(f.feed("hello "), "hello ");
        assert_eq!(f.feed("world"), "world");
        assert_eq!(f.finish(), "");
        assert!(f.calls.is_empty());
    }

    #[test]
    fn extracts_block_split_across_chunks() {
        let mut f = StreamToolFilter::new(Dialect::JsonInvoke);
        assert_eq!(f.feed("Sure. <inv"), "Sure. ");
        assert_eq!(f.feed("oke name=\"bash\">{\"command\":"), "");
        assert_eq!(f.feed("\"ls\"}</invoke>"), "");
        assert_eq!(f.calls.len(), 1);
        assert_eq!(f.calls[0].name, "bash");
        assert_eq!(
            f.calls[0].json().unwrap(),
            serde_json::json!({"command": "ls"})
        );
    }

    #[test]
    fn holds_back_partial_tag_until_finish() {
        let mut f = StreamToolFilter::new(Dialect::JsonInvoke);
        assert_eq!(f.feed("text <inv"), "text ");
        // "<invoke" is still an incomplete opening tag: hold it, do not emit.
        assert_eq!(f.feed("oke"), "");
        // Only at end of stream does it degrade to text.
        assert_eq!(f.finish(), "<invoke");
    }

    #[test]
    fn unterminated_block_is_malformed_at_finish() {
        let mut f = StreamToolFilter::new(Dialect::JsonInvoke);
        assert_eq!(f.feed("a <invoke name=\"x\">{"), "a ");
        assert_eq!(f.finish(), "");
        assert!(f.calls.is_empty());
        assert_eq!(f.malformed.len(), 1);
        assert_eq!(f.malformed[0].name.as_deref(), Some("x"));
    }

    #[test]
    fn swallows_two_extra_closing_tags() {
        let c = INVOKE_CLOSE;
        let mut f = StreamToolFilter::new(Dialect::JsonInvoke);
        let out = f.feed(&format!("<invoke name=\"t\">{{\"a\":1}}{c}{c}{c} ok"));
        assert_eq!(out, " ok");
        assert_eq!(f.calls.len(), 1);
    }

    #[test]
    fn leaves_closing_tags_beyond_the_limit() {
        let c = INVOKE_CLOSE;
        let mut f = StreamToolFilter::new(Dialect::JsonInvoke);
        let out = f.feed(&format!("<invoke name=\"t\">{{\"a\":1}}{c}{c}{c}{c}x"));
        assert_eq!(out, format!("{c}x"));
        assert_eq!(f.calls.len(), 1);
    }

    #[test]
    fn prose_after_block_is_emitted() {
        let mut f = StreamToolFilter::new(Dialect::JsonInvoke);
        let out = f.feed("<invoke name=\"t\">{\"a\":1}</invoke> done");
        assert_eq!(out, " done");
        assert_eq!(f.calls.len(), 1);
    }
}
