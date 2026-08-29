# DeepSeek OpenAI Proxy — Agent Guide

## Overview
FastAPI proxy that translates OpenAI-format `/v1/chat/completions` to the DeepSeek web API via `aiodeepseek`. Supports streaming, tool calling, and conversation threading.

## Key Files
- `main.py` — FastAPI app, request handler, SSE streaming
- `deepseek_client.py` — wraps `aiodeepseek` Conversation objects, JSON session-state persistence
- `streaming_handler.py` — char-level SSE state machine that parses tool blocks out of the token stream
- `tool_parser.py` — tool-call extraction + system-prompt injection
- `config.py` — env vars, token estimation (tiktoken `cl100k_base`)
- `session_state.json` — persists `session_id` and per-thread `parent_message_id` so the proxy reconnects to the same DeepSeek web conversation after restart

## Tool-call wire format
The model is taught (via `inject_tool_descriptions`) to emit tools as XML blocks:

```
<invoke name="tool_name">
<parameter name="param1">value1