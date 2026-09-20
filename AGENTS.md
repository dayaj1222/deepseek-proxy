# DeepSeek OpenAI Proxy — Agent Guide

## Overview
FastAPI proxy that translates OpenAI-format `/v1/chat/completions` to the DeepSeek web API via `aiodeepseek`. Supports streaming, tool calling, and conversation threading.

## Key Files
- `main.py` — FastAPI app, request handler, SSE streaming
- `deepseek_client.py` — wraps `aiodeepseek` Conversation objects, JSON session-state persistence
- `streaming_handler.py` — char-level SSE state machine that parses tool blocks out of the token stream
- `tool_format.py` — SINGLE SOURCE OF TRUTH for the tool-call wire format:
  `DIALECTS` registry + one-switch selection (`TOOL_FORMAT`) + all
  emit/parse/stream helpers. Everything else imports from here.
- `tool_parser.py` — backwards-compat shim re-exporting `tool_format`
- `config.py` — env vars, token estimation (tiktoken `cl100k_base`)
- `session_state.json` — persists `session_id` and per-thread `parent_message_id` so the proxy reconnects to the same DeepSeek web conversation after restart

## Tool-call wire format
Selected by ONE value — `TOOL_FORMAT` env var or `config.toml` key — naming a
`tool_format.DIALECTS` entry (`json_invoke` default, `xml_params`, `dsml`
legacy). Parsing accepts every known spelling regardless; the switch controls
only what is taught/emitted. Current default taught to the model:

```
<invoke name="tool_name">{"param1": "value1", "count": 5}</invoke>
```