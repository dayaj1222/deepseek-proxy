//! DeepSeek wire protocol: SSE parsing and fragment routing.
//!
//! Port of `deepseek_proxy/thinking.py` from the Python implementation.
//! The fragment router is the part that must match byte-for-byte; it is
//! tested against real captured streams in `tests/corpus.rs`.

pub mod fragment;

pub use fragment::{
    classify_line, is_context_limit, is_invalid_message_id, route_event, route_stream, Channel,
    FragmentRouter, LineKind,
    RerouteEvent,
};
