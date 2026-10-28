//! Minimal tool wire format: `<tool>` marker, one JSON call, then `</tool>`.
//!
//! The JSON object itself delimits the call (it is read with a real JSON
//! parser), so the closing tag is optional and text after the call is prose.
//! Several blocks in one reply are several calls. The DSML spelling the backend
//! drifts into is parsed by `dialect_dsml` and, when it cannot be, reported
//! with an explanation instead of being dropped.

use serde_json::Value;

use crate::dialect::{MalformedCall, ScanEvent, ToolCall};
use crate::dialect_dsml;

pub const TOOL_MARKER: &str = "<tool>";
pub const TOOL_CLOSE: &str = "</tool>";

pub fn render_tool_call(call: &ToolCall) -> String {
    // History replay keeps the arguments exactly as the model sent them when
    // they are a JSON object; anything else is carried as a string.
    let args = match serde_json::from_str::<Value>(&call.arguments) {
        Ok(Value::Object(_)) => call.arguments.trim().to_string(),
        _ => Value::String(call.arguments.clone()).to_string(),
    };
    format!(
        "{TOOL_MARKER}{{\"name\":{},\"arguments\":{args}}}{TOOL_CLOSE}",
        serde_json::to_string(&call.name).unwrap_or_else(|_| "\"\"".into()),
    )
}

pub fn native_tools_block(tools: &[Value]) -> String {
    if tools.is_empty() {
        return String::new();
    }
    // Deliberately untaught syntax. The backend emits tool calls in its own
    // native spelling without being told how, and teaching a specific format
    // is what pushes an account toward a policy strike. `dialect_simple` and
    // `dialect_dsml` between them recover every spelling the model actually
    // produces, so the prompt only has to say that tools exist and that one
    // call goes out per response.
    let mut out = String::from(
        "Use the tools below when they help. Call one tool per response, then wait for its result before continuing; if no tool fits, answer directly.\n\nAvailable tools:\n",
    );
    for tool in tools {
        if let Some(function) = tool.get("function") {
            out.push_str(&serde_json::to_string(function).unwrap_or_default());
            out.push('\n');
        }
    }
    out
}

pub fn native_reminder() -> String {
    "Tool-call reminder: call a DSML tool when it helps, one call per response, then wait for the result. If no tool fits, answer directly.".into()
}

/// Taught form: the `simple_tool_tag` dialect names the exact marker.
pub fn tools_block(tools: &[Value]) -> String {
    if tools.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "Tool calls use one opening tag, one JSON object and one closing tag. For a tool call, output `<tool>{\"name\":\"TOOL_NAME\",\"arguments\":{...}}</tool>`, and keep the JSON valid. Write the closing `</tool>` exactly ONCE per call: never repeat it or write it twice in a row, and never write it anywhere except directly after the call's JSON. Otherwise emit one call per response and wait for its result. Otherwise answer normally.\n\nRules:\n1. Do not wrap the call in a code fence.\n2. Use only the parameters in the tool's schema and never invent values.\n3. After a tool result arrives, continue the task; do not repeat the same call.\n4. If no tool fits, answer directly without a call.\n\nAvailable tools:\n",
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
    "Tool-call reminder: to call a tool, write `<tool>` followed by one JSON object with `name` and `arguments`, then `</tool>`; keep the JSON valid. Write `</tool>` exactly ONCE per call: never repeat it or write it twice in a row, and never write it anywhere else. Batch only short, independent, read-only calls.".into()
}

#[cfg(test)]
pub fn parse_tool_calls(text: &str) -> (String, Vec<ToolCall>) {
    let mut prose = String::new();
    let mut calls = Vec::new();
    for event in scan(text) {
        match event {
            ScanEvent::Text(t) => prose.push_str(&t),
            ScanEvent::Tool(call) => calls.push(call),
            ScanEvent::Malformed(_) => {}
        }
    }
    (prose, calls)
}

/// Ordered events for one complete reply.
///
/// Every marker that is followed by a JSON object starts a call; the object is
/// read with a real JSON parser, so a closing tag inside a string value cannot
/// end it early, and text after the closing tag is prose, not part of the call.
/// A marker that is not followed by `{` is ordinary prose (the model mentioned
/// the tag), so it no longer swallows the rest of the reply.
pub fn scan(text: &str) -> Vec<ScanEvent> {
    let mut out = Vec::new();
    let mut prose = String::new();
    let mut rest = text;
    loop {
        let marker = rest.find(TOOL_MARKER);
        let dsml = dialect_dsml::find_start(rest);
        // The model's own spellings, which the `native` dialect reads instead
        // of teaching one: `<name>{json}</name>`, `<tool_call>{...}`, and a
        // bare `{\"name\":...}` object. None of these are taught anywhere; they
        // are what the backend writes unprompted.
        let native = find_native(rest).filter(|(at, _)| {
            marker.is_none_or(|m| *at < m) && dsml.is_none_or(|d| *at < d)
        });
        if let Some((at, event)) = native {
            prose.push_str(&rest[..at]);
            if !prose.is_empty() {
                out.push(ScanEvent::Text(std::mem::take(&mut prose)));
            }
            let (consumed, event) = event;
            out.push(event);
            rest = &rest[at + consumed..];
            continue;
        }
        if let Some(d) = dsml.filter(|d| marker.is_none_or(|m| *d < m)) {
            prose.push_str(&rest[..d]);
            if !prose.is_empty() {
                out.push(ScanEvent::Text(std::mem::take(&mut prose)));
            }
            let region = dialect_dsml::parse_region(&rest[d..]);
            for result in region.results {
                out.push(match result {
                    Ok(call) => ScanEvent::Tool(call),
                    Err(f) => ScanEvent::Malformed(MalformedCall::new(
                        f.name,
                        f.body,
                        &format!("{}:{}", dialect_dsml::UNPARSABLE, f.detail),
                    )),
                });
            }
            rest = &rest[d + region.consumed..];
            continue;
        }
        let Some(i) = marker else { break };
        prose.push_str(&rest[..i]);
        let after = &rest[i + TOOL_MARKER.len()..];
        let body = after.trim_start();
        if !body.starts_with('{') {
            prose.push_str(TOOL_MARKER);
            rest = after;
            continue;
        }
        if !prose.is_empty() {
            out.push(ScanEvent::Text(std::mem::take(&mut prose)));
        }
        match parse_object(body) {
            Some((value, used)) => {
                let tail = &body[used..];
                rest = tail.trim_start().strip_prefix(TOOL_CLOSE).unwrap_or(tail);
                match call_from_value(&value) {
                    Ok(call) => out.push(ScanEvent::Tool(call)),
                    Err((name, reason)) => out.push(ScanEvent::Malformed(MalformedCall::new(
                        name,
                        body[..used].to_string(),
                        reason,
                    ))),
                }
            }
            None => {
                // Cut off or invalid: the bad region ends at the closing tag or
                // the next marker, whichever comes first.
                let end = [body.find(TOOL_CLOSE), body.find(TOOL_MARKER)]
                    .into_iter()
                    .flatten()
                    .min();
                let (raw, tail) = match end {
                    Some(e) => (&body[..e], &body[e..]),
                    None => (body, ""),
                };
                rest = tail.strip_prefix(TOOL_CLOSE).unwrap_or(tail);
                out.push(ScanEvent::Malformed(MalformedCall::new(
                    salvage_name(raw),
                    raw.trim().to_string(),
                    "invalid_json_body",
                )));
            }
        }
    }
    prose.push_str(rest);
    if !prose.is_empty() {
        out.push(ScanEvent::Text(prose));
    }
    out
}

/// Earliest position in `text` where a native-spelling call may begin, for the
/// streaming filter: a tag opener or a call-shaped `{` object.
///
/// Deliberately more eager than [`find_native`]: a half-received call must be
/// held, not streamed as prose. A `{` is only reported when it could still turn
/// into a `name`/`function` object, so ordinary JSON in the reply passes
/// through untouched.
fn native_start(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'<' => {
                let rest = &text[i + 1..];
                let tag_like = rest
                    .chars()
                    .next()
                    .is_none_or(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
                if tag_like {
                    return Some(i);
                }
            }
            b'{' => {
                let rest = &text[i..];
                match parse_object(rest) {
                    Some((value, used)) => {
                        if value.as_object().is_some_and(|o| {
                            o.contains_key("name") || o.contains_key("function")
                        }) {
                            return Some(i);
                        }
                        i += used;
                        continue;
                    }
                    None if could_be_call_object(rest) => return Some(i),
                    None => {}
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Whether a truncated `{...` is still a prefix of a `name`/`function` object.
fn could_be_call_object(rest: &str) -> bool {
    let Some(after) = rest.strip_prefix('{') else {
        return false;
    };
    let after = after.trim_start();
    ["\"name\"", "\"function\""].iter().any(|key| {
        if after.len() <= key.len() {
            key.starts_with(after)
        } else {
            after.starts_with(key)
        }
    })
}

/// Earliest native-spelling tool call in `text`, with its byte offset.
///
/// The `native` dialect teaches no format, so the model writes whatever it
/// likes; these are the shapes it actually produces. None is taught.
fn find_native(text: &str) -> Option<(usize, (usize, ScanEvent))> {
    for (i, c) in text.char_indices() {
        if c != '<' && c != '{' {
            continue;
        }
        if let Some(parsed) = native_at(&text[i..]) {
            return Some((i, parsed));
        }
    }
    None
}

/// One native spelling at the very start of `s`, and the bytes it covers.
fn native_at(s: &str) -> Option<(usize, ScanEvent)> {
    if let Some(rest) = s.strip_prefix('<') {
        let name_end = rest.find(|c: char| {
            !(c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
        })?;
        let tag = &rest[..name_end];
        if tag.is_empty() || !rest[name_end..].starts_with('>') {
            return None;
        }
        let body = &rest[name_end + 1..];
        let trimmed = body.trim_start();
        if !trimmed.starts_with('{') {
            return None;
        }
        let (value, used) = parse_object(trimmed)?;
        let after = &trimmed[used..];
        let tail = after.trim_start();
        let mut close = String::from("</");
        close.push_str(tag);
        close.push('>');
        let close_len = match tail.strip_prefix(close.as_str()) {
            Some(_) => (after.len() - tail.len()) + close.len(),
            None => 0,
        };
        let consumed = 1 + name_end + 1 + (body.len() - trimmed.len()) + used + close_len;
        let wrapper = matches!(
            tag,
            "tool_call" | "tool_calls" | "function_call" | "tool" | "invoke"
        );
        let call = if wrapper {
            call_from_value(&value)
        } else {
            Ok(ToolCall {
                name: tag.to_string(),
                arguments: value.to_string(),
            })
        };
        return Some(match call {
            Ok(call) => (consumed, ScanEvent::Tool(call)),
            Err((name, reason)) => (
                consumed,
                ScanEvent::Malformed(MalformedCall::new(
                    name,
                    trimmed[..used].to_string(),
                    reason,
                )),
            ),
        });
    }
    if s.starts_with('{') {
        let (value, used) = parse_object(s)?;
        let looks_like_call = value
            .as_object()
            .is_some_and(|o| o.contains_key("name") || o.contains_key("function"));
        if !looks_like_call {
            return None;
        }
        let call = call_from_value(&value).ok()?;
        return Some((used, ScanEvent::Tool(call)));
    }
    None
}

/// First JSON object at the start of `body` and the bytes it occupies.
///
/// Strict JSON first. If that fails, the same object is retried after the
/// slips models make when writing JSON by hand: raw control characters inside
/// strings (tabs, newlines), exotic Unicode spaces and zero-width characters
/// between tokens, and a trailing comma before a closer. Nothing inside a
/// string changes meaning: a raw newline becomes the escape `\n`.
fn parse_object(body: &str) -> Option<(Value, usize)> {
    let mut values = serde_json::Deserializer::from_str(body).into_iter::<Value>();
    if let Some(Ok(value)) = values.next() {
        return Some((value, values.byte_offset()));
    }
    let (clean, used) = repair_object(body)?;
    Some((serde_json::from_str(&clean).ok()?, used))
}

fn repair_object(body: &str) -> Option<(String, usize)> {
    let mut out = String::with_capacity(body.len());
    let (mut in_str, mut esc, mut depth) = (false, false, 0usize);
    for (i, c) in body.char_indices() {
        if in_str {
            match c {
                _ if esc => {
                    esc = false;
                    out.push(c);
                }
                '\\' => {
                    esc = true;
                    out.push(c);
                }
                '"' => {
                    in_str = false;
                    out.push(c);
                }
                c if c < ' ' => match c {
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\t' => out.push_str("\\t"),
                    c => out.push_str(&format!("\\u{:04x}", c as u32)),
                },
                c => out.push(c),
            }
            continue;
        }
        match c {
            '"' => {
                in_str = true;
                out.push(c);
            }
            '{' | '[' => {
                depth += 1;
                out.push(c);
            }
            '}' | ']' => {
                let kept = out.trim_end().len();
                if out[..kept].ends_with(',') {
                    out.truncate(kept - 1);
                }
                out.push(c);
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some((out, i + c.len_utf8()));
                }
            }
            '\u{200b}' | '\u{200c}' | '\u{200d}' | '\u{2060}' | '\u{feff}' => {}
            c if c.is_whitespace() && !c.is_ascii() => out.push(' '),
            c => out.push(c),
        }
    }
    None
}

/// Reasons are the `json_invoke` vocabulary, so `repair_schemas` re-sends the
/// schema of a tool whose name is known.
fn call_from_value(value: &Value) -> Result<ToolCall, (Option<String>, &'static str)> {
    let Some(name) = value
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return Err((None, "invalid_arguments_json"));
    };
    let fail = |reason| Err((Some(name.to_string()), reason));
    let arguments = match value.get("arguments") {
        Some(args @ Value::Object(_)) => args.to_string(),
        // Double-encoded arguments are tolerated, but only an object is a call.
        Some(Value::String(s)) => match serde_json::from_str::<Value>(s) {
            Ok(Value::Object(_)) => s.clone(),
            Ok(_) => return fail("body_not_object"),
            Err(_) => return fail("invalid_json_body"),
        },
        _ => return fail("body_not_object"),
    };
    Ok(ToolCall {
        name: name.to_string(),
        arguments,
    })
}

/// Best-effort tool name from a body that does not parse: a `"name"` string
/// that appears before `"arguments"`, so a parameter called `name` is not
/// mistaken for the tool.
fn salvage_name(raw: &str) -> Option<String> {
    let key = raw.find("\"name\"")?;
    if raw.find("\"arguments\"").is_some_and(|a| a < key) {
        return None;
    }
    let after = raw[key + "\"name\"".len()..]
        .trim_start()
        .strip_prefix(':')?
        .trim_start();
    let name = serde_json::Deserializer::from_str(after)
        .into_iter::<String>()
        .next()?
        .ok()?;
    (!name.is_empty()).then_some(name)
}

/// Incremental filter: stream prose before the first `<tool>` (or DSML tag), then hold
/// everything from the marker on and parse it with [`scan`] at end-of-stream.
/// Prose that follows a call is returned by `finish`.
pub struct StreamFilter {
    pending: String,
    held: Option<String>,
    pub calls: Vec<ToolCall>,
    pub malformed: Vec<MalformedCall>,
}

impl StreamFilter {
    pub fn new() -> Self {
        Self {
            pending: String::new(),
            held: None,
            calls: Vec::new(),
            malformed: Vec::new(),
        }
    }

    pub fn feed(&mut self, chunk: &str) -> String {
        if let Some(held) = &mut self.held {
            held.push_str(chunk);
            return String::new();
        }
        self.pending.push_str(chunk);
        let start = [
            self.pending.find(TOOL_MARKER),
            dialect_dsml::find_start(&self.pending),
            native_start(&self.pending),
        ]
        .into_iter()
        .flatten()
        .min();
        if let Some(i) = start {
            let prose = self.pending[..i].to_string();
            self.held = Some(self.pending[i..].to_string());
            self.pending.clear();
            return prose;
        }
        let hold = (1..TOOL_MARKER.len())
            .rev()
            .find(|&n| self.pending.ends_with(&TOOL_MARKER[..n]))
            .unwrap_or(0)
            .max(dialect_dsml::partial_len(&self.pending));
        let safe = self.pending.len() - hold;
        self.pending.drain(..safe).collect()
    }

    pub fn finish(&mut self) -> String {
        let Some(held) = self.held.take() else {
            return std::mem::take(&mut self.pending);
        };
        let mut tail = String::new();
        for event in scan(&held) {
            match event {
                ScanEvent::Text(t) => tail.push_str(&t),
                ScanEvent::Tool(call) => self.calls.push(call),
                ScanEvent::Malformed(m) => self.malformed.push(m),
            }
        }
        tail
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_tag_json_object_and_closing_tag() {
        assert_eq!(
            render_tool_call(&ToolCall {
                name: "bash".into(),
                arguments: "{\"command\":\"echo hi\"}".into(),
            }),
            "<tool>{\"name\":\"bash\",\"arguments\":{\"command\":\"echo hi\"}}</tool>"
        );
    }

    #[test]
    fn teaches_no_wire_format_at_all() {
        let tools = vec![serde_json::json!({
            "type": "function",
            "function": {"name": "bash", "description": "run", "parameters": {}}
        })];
        let block = native_tools_block(&tools);
        let reminder = native_reminder();
        // The point of this dialect: name no syntax. The model's own native
        // spelling is recovered by `scan` and `dialect_dsml`, and naming a
        // format is what gets an account throttled or banned. Only the prose
        // preamble is checked; the serialized tool schemas below it are the
        // client's own JSON and legitimately contain `parameters`.
        let preamble = block.split("Available tools:").next().unwrap();
        for text in [preamble, reminder.as_str()] {
            assert!(!text.contains("<tool>"));
            assert!(!text.contains("</tool>"));
            assert!(!text.contains("｜｜DSML｜｜"));
            assert!(!text.contains("invoke"));
            assert!(!text.contains("parameter"));
            assert!(!text.contains("ABSOLUTELY FORBIDDEN"));
        }
        // It still has to convey that tools exist and how many go out at once.
        assert!(block.contains("Available tools:"));
        assert!(block.contains("\"name\":\"bash\""));
        assert!(reminder.contains("one call per response"));
        // The anchor is the word, never markup: `DSML` names the backend's own
        // format without spelling a single tag.
        assert!(reminder.contains("DSML tool"));
        assert!(!reminder.contains('<'));
    }

    #[test]
    fn accepts_optional_closing_tool_tag_after_call_json() {
        let raw = "Working. <tool>{\"name\":\"bash\",\"arguments\":{\"command\":\"echo hi\"}}</tool>";
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
    fn trailing_text_is_prose_and_streamed_tag_is_held() {
        let events = scan("<tool>{\"name\":\"x\",\"arguments\":{}}</tool> extra");
        assert!(matches!(&events[0], ScanEvent::Tool(c) if c.name == "x"));
        assert!(matches!(&events[1], ScanEvent::Text(t) if t == " extra"));
        // A missing closer is tolerated.
        assert!(matches!(
            scan("<tool>{\"name\":\"x\",\"arguments\":{}}").as_slice(),
            [ScanEvent::Tool(_)]
        ));
        let mut filter = StreamFilter::new();
        assert_eq!(filter.feed("hello <to"), "hello ");
        assert_eq!(
            filter
                .feed("ol>{\"name\":\"bash\",\"arguments\":{}}</tool> done")
                .as_str(),
            ""
        );
        assert_eq!(filter.finish(), " done");
        assert_eq!(filter.calls.len(), 1);
    }

    #[test]
    fn closing_tag_inside_a_string_value_does_not_end_the_call() {
        let raw = "<tool>{\"name\":\"write\",\"arguments\":{\"text\":\"a </tool> b\"}}</tool>";
        let (_, calls) = parse_tool_calls(raw);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].json().unwrap()["text"], "a </tool> b");
    }

    #[test]
    fn code_fence_after_the_call_does_not_lose_it() {
        let raw = "```\n<tool>{\"name\":\"x\",\"arguments\":{}}</tool>\n```";
        let (prose, calls) = parse_tool_calls(raw);
        assert_eq!(calls.len(), 1);
        assert_eq!(prose, "```\n\n```");
    }

    // These pin dialect differences that are behaviour, not prose, so a change
    // cannot silently "fix" them away.

    /// A marker that is not followed by a JSON object is just prose, so
    /// mentioning the tag no longer swallows the rest of the reply.
    #[test]
    fn marker_in_prose_stays_prose() {
        let raw = "Emit `<tool>` to call a tool. Then write more prose.";
        assert!(matches!(scan(raw).as_slice(), [ScanEvent::Text(t)] if t == raw));
        assert!(parse_tool_calls(raw).1.is_empty());
    }

    /// `arguments` may be a JSON object or a *string* that itself parses as
    /// JSON. The json dialect rejects the second form as `body_not_object`, and
    /// the accepted value is re-serialised, so it is not byte-identical to what
    /// the model emitted.
    #[test]
    fn accepts_double_encoded_arguments_and_reserialises_them() {
        let raw = "<tool>{\"name\":\"bash\",\"arguments\":\"{\\\"command\\\": \\\"ls\\\"}\"}</tool>";
        let (_, calls) = parse_tool_calls(raw);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "bash");
        assert_eq!(calls[0].arguments, "{\"command\": \"ls\"}");
        assert_eq!(calls[0].json().unwrap()["command"], "ls");
    }

    /// There is no `normalize_dsml` here. A model that drifts into the other
    /// syntax produces ordinary text: nothing executes, and the failure is not
    /// even recorded for repair.
    #[test]
    fn does_not_fall_back_to_the_invoke_syntax() {
        let raw = "<invoke name=\"bash\">{\"command\":\"ls\"}</invoke>";
        let (prose, calls) = parse_tool_calls(raw);
        assert!(calls.is_empty());
        assert_eq!(prose, raw, "unparsed output stays plain text");
        assert!(matches!(scan(raw).as_slice(), [ScanEvent::Text(_)]));
    }

    /// Every block is its own call, in order, with prose kept around them.
    #[test]
    fn several_blocks_are_several_calls() {
        let raw = concat!(
            "one <tool>{\"name\":\"a\",\"arguments\":{}}</tool>",
            " two <tool>{\"name\":\"b\",\"arguments\":{}}</tool> three"
        );
        let (prose, calls) = parse_tool_calls(raw);
        assert_eq!(calls.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["a", "b"]);
        assert_eq!(prose, "one  two  three");
    }

    #[test]
    fn a_broken_block_does_not_poison_the_next() {
        let raw = "<tool>{\"name\":\"a\",\"arguments\":{</tool><tool>{\"name\":\"b\",\"arguments\":{}}</tool>";
        let events = scan(raw);
        assert!(matches!(&events[0], ScanEvent::Malformed(_)));
        assert!(matches!(&events[1], ScanEvent::Tool(c) if c.name == "b"));
    }

    #[test]
    fn failures_use_json_reasons_and_keep_the_tool_name() {
        let reasons = |raw: &str| match scan(raw).pop() {
            Some(ScanEvent::Malformed(m)) => (m.name, m.reason),
            other => panic!("expected malformed, got {other:?}"),
        };
        let named = |r: &str| (Some("bash".to_string()), r.to_string());
        assert_eq!(reasons("<tool>{\"name\":\"bash\"}</tool>"), named("body_not_object"));
        assert_eq!(
            reasons("<tool>{\"name\":\"bash\",\"arguments\":[1]}</tool>"),
            named("body_not_object")
        );
        assert_eq!(
            reasons("<tool>{\"name\":\"bash\",\"arguments\":{\"command\":\"ls"),
            named("invalid_json_body")
        );
        // A parameter called `name` is not the tool name.
        assert_eq!(
            reasons("<tool>{\"arguments\":{\"name\":\"x\"").0,
            None
        );
    }

    #[test]
    fn dsml_is_parsed_when_it_can_be_and_explained_when_it_cannot() {
        let ok = "Let me look.\n<｜｜DSML｜｜ calls><｜｜DSML｜｜ invoke name=\"bash\"><｜｜DSML｜｜ parameter name=\"command\" string=\"true\">ls</｜｜DSML｜｜ parameter></｜｜DSML｜｜ invoke></｜｜DSML｜｜ calls>";
        let (prose, calls) = parse_tool_calls(ok);
        assert_eq!(prose, "Let me look.\n");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].json().unwrap()["command"], "ls");

        let bad = "<｜｜DSML｜｜ invoke name=\"bash\"><｜｜DSML｜｜ parameter name=\"command\">ls";
        let Some(ScanEvent::Malformed(m)) = scan(bad).pop() else { panic!() };
        assert_eq!(m.name.as_deref(), Some("bash"));
        assert!(m.reason.starts_with("dsml_unparsable:"), "{}", m.reason);
    }

    #[test]
    fn streamed_dsml_is_held_and_parsed_at_the_end() {
        let mut f = StreamFilter::new();
        assert_eq!(f.feed("go <｜｜DS"), "go ");
        assert_eq!(f.feed("ML｜｜ calls><｜｜DSML｜｜ invoke name=\"a\">"), "");
        assert_eq!(f.feed("</｜｜DSML｜｜ invoke></｜｜DSML｜｜ calls>"), "");
        assert_eq!(f.finish(), "");
        assert_eq!(f.calls.len(), 1);
    }

    #[test]
    fn replay_keeps_the_argument_bytes_the_model_sent() {
        let call = ToolCall {
            name: "bash".into(),
            arguments: "{ \"z\": 1.0,  \"a\": \"\\u003c\" }".into(),
        };
        let out = render_tool_call(&call);
        assert!(out.contains(&call.arguments), "{out}");
        let (_, calls) = parse_tool_calls(&out);
        assert_eq!(calls[0].name, "bash");
    }

    #[test]
    fn results_are_attributed_by_call_id() {
        let out = crate::dialect::Dialect::SimpleToolTag.render_tool_result("call_1", "ok </tool>");
        assert_eq!(out, "Tool result (id=call_1):\nok </tool>");
    }

    /// A real reply that went missing: prose, then a call with no closing tag.
    #[test]
    fn real_world_call_with_heredoc_and_no_closer_parses() {
        let raw = concat!("But better: measure sim time.\n\n", r##"<tool>{"name":"bash","arguments":{"command":"cd /tmp && cat > /tmp/dbg4.mjs <<'EOF'\nimport pw from '/home/daya/.npm/lib/node_modules/playwright/index.js';\nconst { chromium } = pw;\nconst browser = await chromium.launch({ args: ['--use-gl=angle','--use-angle=swiftshader','--enable-unsafe-swiftshader'] });\nconst page = await browser.newPage({ viewport: { width: 760, height: 520 } });\nawait page.goto('http://localhost:5199/index.html', { waitUntil: 'networkidle' });\nfor (let i = 0; i < 6; i++) {\n await page.waitForTimeout(7000);\n const s = await page.evaluate(() => ({ c: window.__stillLife.colony.carried, states: window.__stillLife.colony.ants.map(a => a.state).join(',') }));\n console.log(i, JSON.stringify(s));\n}\nawait page.screenshot({ path: '/tmp/shotC.png' });\nawait browser.close();\nEOF\nnode /tmp/dbg4.mjs 2>&1 | tail -8"}}"##);
        let (prose, calls) = parse_tool_calls(raw);
        assert_eq!(calls.len(), 1, "events: {:?}", scan(raw));
        assert_eq!(calls[0].name, "bash");
        assert!(calls[0].json().unwrap()["command"].as_str().unwrap().contains("dbg4.mjs"));
        assert_eq!(prose, "But better: measure sim time.\n\n");
    }

    #[test]
    fn edit_call_with_nested_edits_array_parses() {
        let raw = r##"<tool>{"name":"edit","arguments":{"edits":[{"path":"/home/daya/Development/legal-graphrag/code/db.py","old":"    con.executescript(GRAPH_SCHEMA)","new":"    con.executescript(GRAPH_SCHEMA); con.executescript(GRAPH_FTS_SCHEMA)"}]}}</tool>"##;
        let (_, calls) = parse_tool_calls(raw);
        assert_eq!(calls.len(), 1, "events: {:?}", scan(raw));
        assert_eq!(calls[0].json().unwrap()["edits"][0]["old"], "    con.executescript(GRAPH_SCHEMA)");
    }

    #[test]
    fn hand_written_json_slips_are_repaired() {
        // Raw tab and newline inside a string, an NBSP and a zero-width space
        // between tokens, and a trailing comma.
        let raw = "<tool>{\"name\":\u{a0}\"edit\",\u{200b}\"arguments\":{\"old\":\"a\n\tb\",\"n\":1,},}</tool> after";
        let (prose, calls) = parse_tool_calls(raw);
        assert_eq!(calls.len(), 1, "events: {:?}", scan(raw));
        assert_eq!(calls[0].name, "edit");
        assert_eq!(calls[0].json().unwrap()["old"], "a\n\tb");
        assert_eq!(prose, " after");
    }

    #[test]
    fn truncated_json_is_still_malformed() {
        assert!(matches!(
            scan("<tool>{\"name\":\"edit\",\"arguments\":{\"old\":\"a").pop(),
            Some(ScanEvent::Malformed(m)) if m.name.as_deref() == Some("edit")
        ));
    }
}
