# DeepSeek OpenAI Proxy — Agent Guide

## Overview
FastAPI proxy that translates OpenAI-format `/v1/chat/completions` to DeepSeek API via `aiodeepseek`. Supports streaming, tool calling, and conversation threading.

## Key Files
- `main.py` — FastAPI app, request handler, SSE streaming
- `deepseek_client.py` — wraps `aiodeepseek` Conversation objects, JSON session state persistence
- `streaming_handler.py` — char-level SSE state machine that parses `<invoke>` tool blocks out of the token stream
- `tool_parser.py` — tool call extraction + system prompt injection
- `config.py` — env vars, token estimation (tiktoken cl100k_base)
- `session_state.json` — persists `session_id` and per-thread `parent_message_id` so proxy reconnects to same DeepSeek web conversation after restart

## Architecture
- **No SQLite.** Hermes sends full message history every request, so only `session_id` + per-thread `parent_message_id` are persisted (JSON file).
- **Session state:** `{"session_id": "sess_abc", "threads": {"thread_abc": {"session_id": ..., "parent_message_id": ..., "total_tokens": ..., "exchanges": ...}}}` — held in memory during runtime, flushed to disk atomically on shutdown/SIGHUP/SIGQUIT.
- **Tool calls** use `<invoke name="tool">...