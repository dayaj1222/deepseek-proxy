//! Prompt assembly: the ordering rules for one turn, in one place.
//!
//! Before this module the order was spread over six helpers in `main.rs`
//! (`flatten_transcript`, `delta_messages`, `continuation_prompt`,
//! `tool_choice_directive`, `build_prompts`), so "where does the reminder go?"
//! and "does the image note survive a rebuild?" were answered by reading all of
//! them. The rendering helpers stay where they are: this module owns *ordering*,
//! `main::render_message` owns how one message becomes text.

use serde_json::Value;

use crate::dialect;
use crate::{continuation_prompt, delta_messages, flatten_transcript, Msg};

/// Both prompts for one turn.
///
/// `delta` is what the live session needs now; `replay` is the flattened
/// transcript used to rebuild a session after a context overflow. They share
/// every contribution, so a note added to one can never be missing from the
/// other.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prompt {
    pub delta: String,
    pub replay: String,
}

/// Everything prompt assembly needs, without borrowing the whole request.
pub struct Turn<'a> {
    pub messages: &'a [Msg],
    /// True when the thread already has a remote session, so only the delta is
    /// sent and the protocol reminder is not re-taught at the head.
    pub continuation: bool,
    pub system_extra: &'a str,
    /// Told to the model when a turn carried more images than were sent.
    pub image_note: Option<&'a str>,
    /// `Some` when the turn offers tools, which is what turns the reminder on.
    pub reminder: Option<&'a str>,
    /// `tool_choice` / `parallel_tool_calls`, already resolved.
    pub directive: Option<String>,
}

/// Build both prompts, applying the ordering rules exactly once.
///
/// Order, outermost last, identical to the Python proxy:
/// 1. transcript, then `system_extra` (tool definitions);
/// 2. delta = continuation prompt for an existing thread, else the replay;
/// 3. image note, in both;
/// 4. protocol reminder — at the head of a fresh thread and at the tail of
///    every turn;
/// 5. the `tool_choice` directive last of all.
pub fn for_turn(turn: &Turn<'_>) -> Prompt {
    let transcript = flatten_transcript(turn.messages);
    let mut replay = if turn.system_extra.is_empty() {
        transcript
    } else {
        format!("{transcript}\n\n{}", turn.system_extra)
    };
    let mut delta = if turn.continuation {
        continuation_prompt(delta_messages(turn.messages)).unwrap_or_default()
    } else {
        replay.clone()
    };

    // The image notice goes in both prompts: `replay` is what rebuilds a
    // session after a context overflow, so dropping it there would silently
    // hide the loss from the model on the rebuild.
    if let Some(note) = turn.image_note {
        delta = format!("{note}\n\n{delta}");
        replay = format!("{note}\n\n{replay}");
    }

    // Re-anchor the wire format on every turn: at the head of a fresh thread
    // (nothing has been taught yet) and at the tail of every turn (closest to
    // the generation, where drift happens).
    if let Some(reminder) = turn.reminder {
        if !turn.continuation {
            for prompt in [&mut delta, &mut replay] {
                *prompt = format!("{reminder}\n\n{prompt}");
            }
        }
        delta.push_str("\n\n");
        delta.push_str(reminder);
        replay.push_str("\n\n");
        replay.push_str(reminder);
    }

    if let Some(directive) = &turn.directive {
        delta.push_str(directive);
        replay.push_str(directive);
    }

    Prompt { delta, replay }
}

/// The extra prompt lines for `tool_choice` / `parallel_tool_calls`.
///
/// Port of the block at the top of `ChatService.events()`. `offered` is whether
/// the *filtered* tool list is non-empty, so `tool_choice: "none"` takes the
/// `elif` branch rather than the `if`.
pub fn directive(
    offered: bool,
    required: bool,
    parallel: bool,
    choice_none: bool,
) -> Option<String> {
    let mut out = String::new();
    if offered {
        if required {
            out.push_str("\nYou must return a tool call.");
        }
        if !parallel {
            out.push_str("\nReturn at most one tool call.");
        }
    } else if choice_none {
        out.push_str("\nDo not call tools in this turn; reply with normal text.");
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// `tool_choice` narrowed to the flag pair prompt assembly and recovery need.
pub fn choice_none(tool_choice: Option<&Value>) -> bool {
    matches!(tool_choice, Some(Value::String(s)) if s == "none")
}

/// Reminder text to append, or `None` when the turn offers no tools.
pub fn reminder(tools: &[Value]) -> Option<String> {
    if tools.is_empty() {
        None
    } else {
        Some(dialect::periodic_reminder())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn msg(role: &str, content: &str) -> Msg {
        Msg {
            role: role.into(),
            content: Some(json!(content)),
            tool_calls: Vec::new(),
            tool_call_id: None,
            name: None,
        }
    }

    fn turn<'a>(messages: &'a [Msg], continuation: bool, reminder: Option<&'a str>) -> Turn<'a> {
        Turn {
            messages,
            continuation,
            system_extra: "",
            image_note: None,
            reminder,
            directive: None,
        }
    }

    #[test]
    fn directive_table_matches_the_request_contract() {
        assert_eq!(directive(false, false, true, false), None);
        assert_eq!(
            directive(true, true, true, false).unwrap(),
            "\nYou must return a tool call."
        );
        assert_eq!(
            directive(true, false, false, false).unwrap(),
            "\nReturn at most one tool call."
        );
        assert_eq!(
            directive(false, false, true, true).unwrap(),
            "\nDo not call tools in this turn; reply with normal text."
        );
    }

    #[test]
    fn image_note_reaches_both_prompts() {
        let messages = [msg("user", "hi")];
        let mut t = turn(&messages, false, None);
        t.image_note = Some("[attachment notice]");
        let out = for_turn(&t);
        assert!(out.delta.starts_with("[attachment notice]"));
        assert!(out.replay.starts_with("[attachment notice]"));
    }

    #[test]
    fn reminder_is_anchored_at_head_and_tail_of_a_fresh_thread() {
        let messages = [msg("user", "hi")];
        let out = for_turn(&turn(&messages, false, Some("REM")));
        assert!(out.delta.starts_with("REM"));
        assert!(out.delta.ends_with("REM"));
        assert!(out.replay.starts_with("REM"));
        assert!(out.replay.ends_with("REM"));
    }

    #[test]
    fn continuation_keeps_the_reminder_last_and_only_once_at_the_head() {
        let messages = [msg("user", "again")];
        let out = for_turn(&turn(&messages, true, Some("REM")));
        assert!(out.delta.starts_with("User: again"));
        assert!(out.delta.ends_with("REM"));
        assert_eq!(out.delta.matches("REM").count(), 1);
    }

    #[test]
    fn directive_comes_after_the_reminder() {
        let messages = [msg("user", "hi")];
        let mut t = turn(&messages, true, Some("REM"));
        t.directive = Some("\nYou must return a tool call.".into());
        let out = for_turn(&t);
        assert!(out.delta.ends_with("\nYou must return a tool call."));
    }

    #[test]
    fn no_tools_means_no_reminder() {
        assert!(reminder(&[]).is_none());
        assert!(reminder(&[json!({})]).is_some());
    }
}
