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

pub fn tools_block(tools: &[Value]) -> String {
    if tools.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "Tool calls use one opening tag, one JSON object and one closing tag. For a tool call, output `<tool>{\"name\":\"TOOL_NAME\",\"arguments\":{...}}</tool>`, and keep the JSON valid. You may emit several complete blocks back to back only for short, independent, read-only calls; otherwise emit one call per response and wait for its result. Otherwise answer normally. The DSML dialect is ABSOLUTELY FORBIDDEN: never write `｜｜DSML｜｜` or any tag related to it, whatever follows it; never use XML-style wrappers such as a calls wrapper, tool_calls, invoke or parameter, in any spelling. That syntax will not run and will be rejected. The `<tool>` marker is the only tool-call syntax, with no exceptions.\n\nRules:\n1. Do not wrap the call in a code fence.\n2. Use only the parameters in the tool's schema and never invent values.\n3. After a tool result arrives, continue the task; do not repeat the same call.\n4. If no tool fits, answer directly without a call.\n\nAvailable tools:\n",
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
    "Tool-call reminder: to call a tool, write `<tool>` followed by one JSON object with `name` and `arguments`, then `</tool>`; keep the JSON valid. Batch only short, independent, read-only calls. ABSOLUTELY FORBIDDEN: the DSML dialect. Never write `｜｜DSML｜｜` or any tag related to it, whatever follows it. Never write a calls wrapper, an invoke tag, a parameter tag or a tool_calls block, in any spelling, with or without spaces. That syntax is NOT understood here: it will not run, it will be rejected, and you will have to redo the call. If you feel pulled toward it, stop and write `<tool>{\"name\":\"TOOL_NAME\",\"arguments\":{...}}</tool>` instead. The `<tool>` marker is the ONLY tool-call syntax, with no exceptions.".into()
}

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
    fn teaches_and_reminds_the_closing_tag() {
        let tools = vec![serde_json::json!({
            "type": "function",
            "function": {"name": "bash", "description": "run", "parameters": {}}
        })];
        assert!(tools_block(&tools).contains("</tool>`"));
        assert!(reminder().contains("</tool>"));
        assert!(!reminder().contains("Do not add a closing tag"));
    }

    #[test]
    fn teaches_that_the_dsml_dialect_is_forbidden() {
        let tools = vec![serde_json::json!({
            "type": "function",
            "function": {"name": "bash", "description": "run", "parameters": {}}
        })];
        // Both the one-time block and the per-turn reminder must rule it out,
        // because the backend tends to fall back to DSML on its own.
        assert!(tools_block(&tools).contains("ABSOLUTELY FORBIDDEN"));
        assert!(reminder().contains("ABSOLUTELY FORBIDDEN: the DSML dialect"));
        assert!(reminder().contains("`｜｜DSML｜｜` or any tag related to it"));
        assert!(tools_block(&tools).contains("`｜｜DSML｜｜` or any tag related to it"));
        assert!(reminder().contains("tool_calls block"));
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

    // The differences documented in `docs/features/T-23-tool-dialects.md` are
    // behaviour, not prose: these tests pin the ones a change could silently
    // "fix" into disagreeing with the docs.

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
