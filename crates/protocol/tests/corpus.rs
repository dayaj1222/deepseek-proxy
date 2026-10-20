//! Replay captured SSE streams and assert the Rust router matches the Python
//! implementation byte-for-byte.
//!
//! `corpus/*.expected.json` is produced by `tools/gen_expected.py`, which runs
//! the Python router over `corpus/*.sse`. Python is the oracle.

use protocol::{route_stream, RerouteEvent};
use std::path::{Path, PathBuf};

fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../corpus")
        .canonicalize()
        .expect("corpus directory missing")
}

fn cases() -> Vec<(String, PathBuf, PathBuf)> {
    let dir = corpus_dir();
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("read corpus dir") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("sse") {
            continue;
        }
        let expected = path.with_extension("expected.json");
        assert!(
            expected.exists(),
            "missing {}; run `../deepseek/.venv/bin/python tools/gen_expected.py`",
            expected.display()
        );
        out.push((
            path.file_stem().unwrap().to_string_lossy().into_owned(),
            path,
            expected,
        ));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    assert!(!out.is_empty(), "no corpus fixtures found");
    out
}

#[test]
fn matches_python_router_on_all_captured_streams() {
    for (name, sse, expected_path) in cases() {
        let raw = std::fs::read_to_string(&sse).expect("read sse");
        let expected: Vec<RerouteEvent> =
            serde_json::from_str(&std::fs::read_to_string(&expected_path).expect("read expected"))
                .expect("parse expected json");

        let actual = route_stream(&raw);

        assert_eq!(
            actual.len(),
            expected.len(),
            "{name}: event count differs (rust {} vs python {})",
            actual.len(),
            expected.len()
        );

        for (i, (a, e)) in actual.iter().zip(expected.iter()).enumerate() {
            assert_eq!(a, e, "{name}: event {i} differs");
        }

        let reasoning: usize = actual
            .iter()
            .filter(|e| matches!(e.channel, protocol::Channel::Reasoning))
            .map(|e| e.text.len())
            .sum();
        let text: usize = actual
            .iter()
            .filter(|e| matches!(e.channel, protocol::Channel::Text))
            .map(|e| e.text.len())
            .sum();
        println!(
            "{name}: {} events ok (reasoning={reasoning} chars, text={text} chars)",
            actual.len()
        );
    }
}

#[test]
fn routed_text_reassembles_to_answer() {
    // Sanity: concatenating the text channel must yield the model's answer,
    // not contain any reasoning text.
    let raw = std::fs::read_to_string(corpus_dir().join("think.sse")).expect("read think.sse");
    let events = route_stream(&raw);
    let answer: String = events
        .iter()
        .filter(|e| matches!(e.channel, protocol::Channel::Text))
        .map(|e| e.text.as_str())
        .collect();
    assert!(!answer.is_empty(), "answer channel was empty");
    assert!(answer.len() < 200, "answer suspiciously long: {answer:?}");
}
