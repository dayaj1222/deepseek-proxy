//! Parser for the DSML tool-call spelling the backend drifts into on its own.
//!
//! ```text
//! <｜｜DSML｜｜ calls>
//!  <｜｜DSML｜｜ invoke name="bash">
//!  <｜｜DSML｜｜ parameter name="command" string="true">ls</｜｜DSML｜｜ parameter>
//!  </｜｜DSML｜｜ invoke>
//! </｜｜DSML｜｜ calls>
//! ```
//!
//! It is not a dialect the proxy teaches; it is recovery for a model that
//! ignores the teaching. A region that parses becomes ordinary calls. One that
//! does not becomes a malformed call whose reason says exactly what was wrong,
//! so the repair turn can tell the model what it did and not to do it again.
//!
//! Spelling is deliberately loose: pipes (`｜` or `|`), spaces and case inside
//! the marker are ignored, and a tag may be split across lines.

use serde_json::{Map, Value};

use crate::dialect::ToolCall;

/// Reason prefix of a DSML region that could not be turned into a call. The
/// text after the colon is the human-readable detail.
pub const UNPARSABLE: &str = "dsml_unparsable";

/// Longest tag we will look across for a closing `>`.
const MAX_TAG: usize = 400;
const MAX_BODY: usize = 1500;

#[derive(Debug, PartialEq)]
pub struct Failure {
    pub name: Option<String>,
    pub body: String,
    pub detail: String,
}

#[derive(Debug)]
pub struct Region {
    /// Bytes of the input this region covers (always at least one tag).
    pub consumed: usize,
    pub results: Vec<Result<ToolCall, Failure>>,
}

#[derive(Debug, PartialEq)]
enum Kind {
    Wrapper,
    Invoke,
    Parameter,
}

struct Tag<'a> {
    kind: Kind,
    closing: bool,
    /// Text between `<` and `>`, original spelling.
    inner: &'a str,
    /// Byte offset just past the `>`.
    end: usize,
}

/// The DSML tag starting at `at` (which must be a `<`), if it is one.
fn tag_at(text: &str, at: usize) -> Option<Tag<'_>> {
    let body = &text[at + 1..];
    let gt = body.char_indices().take(MAX_TAG).find(|(_, c)| *c == '>')?.0;
    let inner = &body[..gt];
    let compact: String = inner
        .chars()
        .filter(|c| !matches!(c, '｜' | '|') && !c.is_whitespace())
        .collect::<String>()
        .to_lowercase();
    let (closing, rest) = match compact.strip_prefix("/dsml") {
        Some(r) => (true, r),
        None => (false, compact.strip_prefix("dsml")?),
    };
    let kind = if rest.starts_with("invoke") {
        Kind::Invoke
    } else if rest.starts_with("parameter") {
        Kind::Parameter
    } else {
        Kind::Wrapper
    };
    Some(Tag {
        kind,
        closing,
        inner,
        end: at + 1 + gt + 1,
    })
}

/// Byte offset of the first DSML tag in `text`.
pub fn find_start(text: &str) -> Option<usize> {
    text.match_indices('<')
        .map(|(i, _)| i)
        .find(|&i| tag_at(text, i).is_some())
}

/// Length of a trailing, still-open `<…` that may turn into a DSML tag once
/// more text arrives, so a streaming caller can hold it back.
pub fn partial_len(text: &str) -> usize {
    let Some(at) = text.rfind('<') else { return 0 };
    let tail = &text[at..];
    if tail.contains('>') || tail.len() > MAX_TAG {
        return 0;
    }
    let compact: String = tail[1..]
        .chars()
        .filter(|c| !matches!(c, '｜' | '|') && !c.is_whitespace())
        .collect::<String>()
        .to_lowercase();
    if compact.strip_prefix('/').unwrap_or(&compact).starts_with("dsml")
        || "dsml".starts_with(compact.strip_prefix('/').unwrap_or(&compact))
    {
        tail.len()
    } else {
        0
    }
}

fn attr(inner: &str, key: &str) -> Option<String> {
    let mut from = 0;
    while let Some(rel) = inner[from..].find(key) {
        let at = from + rel;
        from = at + key.len();
        let before_ok = inner[..at]
            .chars()
            .next_back()
            .is_none_or(|c| c.is_whitespace() || c == '｜' || c == '|');
        let Some(rest) = inner[at + key.len()..].trim_start().strip_prefix('=') else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(quote) = rest.chars().next().filter(|c| matches!(c, '"' | '\'')) else {
            continue;
        };
        if !before_ok {
            continue;
        }
        let inner_rest = &rest[1..];
        return inner_rest.find(quote).map(|e| inner_rest[..e].to_string());
    }
    None
}

fn clip(s: &str) -> String {
    let s = s.trim();
    if s.len() <= MAX_BODY {
        return s.to_string();
    }
    let mut end = MAX_BODY;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

struct Open {
    name: Option<String>,
    args: Map<String, Value>,
    start: usize,
}

struct Param {
    name: Option<String>,
    string: bool,
    value: String,
}

fn finish_param(param: Param, call: Option<&mut Open>) {
    let (Some(call), Some(name)) = (call, param.name) else {
        return;
    };
    let value = if param.string {
        Value::String(param.value)
    } else {
        let t = param.value.trim();
        serde_json::from_str(t).unwrap_or_else(|_| Value::String(t.to_string()))
    };
    call.args.insert(name, value);
}

fn finish_call(call: Open, text: &str, end: usize) -> Result<ToolCall, Failure> {
    match call.name.filter(|n| !n.is_empty()) {
        Some(name) => Ok(ToolCall {
            name,
            arguments: Value::Object(call.args).to_string(),
        }),
        None => Err(Failure {
            name: None,
            body: clip(&text[call.start..end]),
            detail: "an invoke tag had no name attribute".into(),
        }),
    }
}

/// Parse the DSML region at the start of `text` (which must begin with a DSML
/// tag, see [`find_start`]).
pub fn parse_region(text: &str) -> Region {
    let mut results = Vec::new();
    let mut call: Option<Open> = None;
    let mut param: Option<Param> = None;
    let mut pos = 0;
    let mut consumed = 0;
    let mut saw_open = false;
    let mut saw_invoke = false;

    loop {
        let Some(rel) = text[pos..].find('<') else {
            // No more tags. Text after the last tag is only ours inside a
            // parameter, and then the region is cut off.
            if param.is_some() || call.is_some() {
                consumed = text.len();
            }
            break;
        };
        let lt = pos + rel;
        let between = &text[pos..lt];
        if param.is_none() && !between.trim().is_empty() {
            break; // prose: the region is over
        }
        let tag = tag_at(text, lt);
        let Some(tag) = tag else {
            match &mut param {
                // A literal `<` in a value, such as `<<EOF` or `a < b`.
                Some(p) => {
                    p.value.push_str(between);
                    p.value.push('<');
                    pos = lt + 1;
                    continue;
                }
                None => break,
            }
        };
        if let Some(p) = &mut param {
            p.value.push_str(between);
        }
        pos = tag.end;
        match (tag.kind, tag.closing) {
            (Kind::Wrapper, false) => {
                saw_open = true;
                consumed = tag.end;
            }
            (Kind::Wrapper, true) => {
                consumed = tag.end;
                if let Some(c) = call.take() {
                    let _ = c; // unclosed invoke before the wrapper closed
                    results.push(Err(Failure {
                        name: None,
                        body: clip(&text[..lt]),
                        detail: "an invoke tag was never closed".into(),
                    }));
                }
                break;
            }
            (Kind::Invoke, false) => {
                saw_open = true;
                saw_invoke = true;
                if let Some(prev) = call.take() {
                    results.push(Err(Failure {
                        name: prev.name,
                        body: clip(&text[prev.start..lt]),
                        detail: "an invoke tag was never closed".into(),
                    }));
                }
                param = None;
                call = Some(Open {
                    name: attr(tag.inner, "name"),
                    args: Map::new(),
                    start: lt,
                });
                consumed = tag.end;
            }
            (Kind::Invoke, true) => {
                if let Some(p) = param.take() {
                    finish_param(p, call.as_mut());
                }
                if let Some(c) = call.take() {
                    results.push(finish_call(c, text, tag.end));
                }
                consumed = tag.end;
            }
            (Kind::Parameter, false) => {
                saw_open = true;
                if let Some(p) = param.take() {
                    finish_param(p, call.as_mut());
                }
                if call.is_none() {
                    results.push(Err(Failure {
                        name: None,
                        body: clip(&text[lt..]),
                        detail: "a parameter tag appeared outside any invoke tag".into(),
                    }));
                }
                param = Some(Param {
                    name: attr(tag.inner, "name"),
                    string: attr(tag.inner, "string").is_some_and(|s| s == "true"),
                    value: String::new(),
                });
                consumed = tag.end;
            }
            (Kind::Parameter, true) => {
                if let Some(p) = param.take() {
                    finish_param(p, call.as_mut());
                }
                consumed = tag.end;
            }
        }
    }

    if let Some(c) = call.take() {
        // Cut off before the invoke closed (usually the token limit).
        results.push(Err(Failure {
            name: c.name.filter(|n| !n.is_empty()),
            body: clip(&text[c.start..]),
            detail: "the output ended before the invoke tag was closed".into(),
        }));
    } else if saw_open && !saw_invoke && results.is_empty() {
        results.push(Err(Failure {
            name: None,
            body: clip(&text[..consumed.max(1).min(text.len())]),
            detail: "the calls wrapper held no invoke tag".into(),
        }));
    }
    Region {
        consumed: consumed.max(text.find('>').map_or(1, |g| g + 1)),
        results,
    }
}

/// What the model is told when a DSML region could not be parsed.
pub fn explanation(detail: &str) -> String {
    format!(
        "You wrote the DSML tool-call syntax (tags such as `<｜｜DSML｜｜ invoke>` and \
         `<｜｜DSML｜｜ parameter>`). That is NOT the syntax for this conversation, and it \
         could not be run because {detail}. Do not use DSML, calls, invoke or parameter \
         tags again. Write the call as `<tool>{{\"name\":\"TOOL_NAME\",\"arguments\":{{...}}}}</tool>`."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "<｜｜DSML｜｜ calls>\n <｜｜DSML｜｜ invoke name=\"bash\">\n <｜｜DSML｜｜ parameter name=\"command\" string=\"true\">ls ~/x 2>/dev/null | head; echo '--- net ---';\n timeout 10 curl -sS</｜｜DSML｜｜\n parameter>\n </｜｜DSML｜｜ invoke>\n </｜｜DSML｜｜ calls>";

    #[test]
    fn parses_the_observed_spelling() {
        assert_eq!(find_start(SAMPLE), Some(0));
        let r = parse_region(SAMPLE);
        assert_eq!(r.consumed, SAMPLE.len());
        let [Ok(call)] = r.results.as_slice() else {
            panic!("{:?}", r.results)
        };
        assert_eq!(call.name, "bash");
        let args = call.json().unwrap();
        assert!(args["command"].as_str().unwrap().starts_with("ls ~/x 2>/dev/null"));
        assert!(args["command"].as_str().unwrap().ends_with("curl -sS"));
    }

    #[test]
    fn non_string_parameters_are_json_and_literal_lt_survives() {
        let t = "<|DSML| invoke name=\"w\"><|DSML| parameter name=\"n\">5</|DSML| parameter><|DSML| parameter name=\"s\" string=\"true\">a <<EOF b</|DSML| parameter></|DSML| invoke>";
        let r = parse_region(t);
        let [Ok(call)] = r.results.as_slice() else { panic!("{:?}", r.results) };
        let args = call.json().unwrap();
        assert_eq!(args["n"], 5);
        assert_eq!(args["s"], "a <<EOF b");
    }

    #[test]
    fn several_invokes_and_trailing_prose() {
        let t = "<｜｜DSML｜｜ calls><｜｜DSML｜｜ invoke name=\"a\"></｜｜DSML｜｜ invoke><｜｜DSML｜｜ invoke name=\"b\"></｜｜DSML｜｜ invoke></｜｜DSML｜｜ calls> after";
        let r = parse_region(t);
        assert_eq!(r.results.len(), 2);
        assert_eq!(&t[r.consumed..], " after");
    }

    #[test]
    fn unparsable_regions_say_why() {
        let cut = parse_region("<｜｜DSML｜｜ invoke name=\"bash\"><｜｜DSML｜｜ parameter name=\"command\">ls");
        let [Err(f)] = cut.results.as_slice() else { panic!() };
        assert_eq!(f.name.as_deref(), Some("bash"));
        assert!(f.detail.contains("ended before"));
        let nameless = parse_region("<｜｜DSML｜｜ invoke></｜｜DSML｜｜ invoke>");
        assert!(matches!(nameless.results.as_slice(), [Err(f)] if f.detail.contains("no name")));
        let empty = parse_region("<｜｜DSML｜｜ calls></｜｜DSML｜｜ calls>");
        assert!(matches!(empty.results.as_slice(), [Err(f)] if f.detail.contains("no invoke")));
        // A stray closer alone is swallowed without a failure.
        assert!(parse_region("</｜｜DSML｜｜ calls>").results.is_empty());
    }

    #[test]
    fn partial_tags_are_held_for_streaming() {
        assert_eq!(partial_len("hi <"), 1);
        assert_eq!(partial_len("hi <｜｜DS"), "<｜｜DS".len());
        assert_eq!(partial_len("hi <｜｜DSML｜｜ inv"), "<｜｜DSML｜｜ inv".len());
        assert_eq!(partial_len("a < b"), 0);
        assert_eq!(partial_len("<b>bold"), 0);
    }
}
