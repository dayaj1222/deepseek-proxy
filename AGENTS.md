# DeepSeek OpenAI Proxy — Agent Guide

## Overview
FastAPI proxy that translates OpenAI-format `/v1/chat/completions` to DeepSeek API via `aiodeepseek`. Supports streaming, tool calling, and conversation threading.

## Key Files
- `main.py` — FastAPI app, request handler, SSE streaming
- `deepseek_client.py` — wraps `aiodeepseek` Conversation objects, JSON session state persistence
- `streaming_handler.py` — SSE generator with tool marker detection
- `tool_parser.py` — tool call extraction + system prompt injection
- `config.py` — env vars, token estimation
- `session_state.json` — persists `session_id` and per-thread `parent_message_id` so proxy reconnects to same DeepSeek web conversation after restart

## Architecture
- **No SQLite.** Hermes sends full message history every request, so only `session_id` + per-thread `parent_message_id` are persisted (JSON file).
- **Session state:** `{"session_id": "sess_abc", "threads": {"thread_abc": "msg_001"}}` — saved after each API response.
- **Tool calls** use `⟿` marker character to detect tool output vs text
- **Streaming:** hybrid generator flushes text at 200 chars, detects complete tool calls in buffer
- **Only new messages** (post-last-assistant) are sent to DeepSeek per turn
- **System prompt** only included on first turn of a conversation (`include_system=is_first`)
- **Threading:** thread_id derived from SHA256 of first user message content; `get_new_messages()` strips history after last assistant; saved `parent_message_id` is injected on cold threads so DeepSeek Conversation already has context.

## Config (`config.py`)
- `REQUEST_DELAY` — seconds to `asyncio.sleep` before each DeepSeek API call; set via `REQUEST_DELAY` in `.env` (default `0`)
- `TOOL_BUFFER_LIMIT` — max bytes to buffer for a complete tool call before falling back to plain text (default `100000`)

## Commands
```bash
uv run main.py              # start server (default port 8000)
uv sync                     # install deps
```

## Conventions
- Tool call format: `⟿{"function": "name", "arguments": {...}}⟿`
- `estimate_tokens(text)` = `len(text) // 4` (rough heuristic)
- Logging at `DEBUG` level with timestamp format
