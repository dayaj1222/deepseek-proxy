# DeepSeek OpenAI Proxy

A FastAPI proxy that exposes an OpenAI-compatible `/v1/chat/completions` endpoint backed by the DeepSeek web API (`aiodeepseek`). Supports streaming, tool calling, and conversation threading across restarts.

## Features

- **OpenAI wire compatibility** — drop-in `base_url` for tools that speak the OpenAI chat-completions schema.
- **Streaming** — server-sent events with char-level tool-call detection.
- **Tool calling** — XML `<invoke>`/`<parameter>` blocks translated to OpenAI `tool_calls` objects.
- **Conversation persistence** — one DeepSeek chat session per thread, reconnected after restart via `session_state.json`.
- **No database** — full history is sent by the client each request; only session metadata is persisted.

## Setup

```bash
# create and activate a virtualenv
uv venv
source .venv/bin/activate

# install dependencies
uv pip install -r requirements.txt
```

## Configuration

Copy `.env.example` (or set the following environment variables):

| Variable | Description | Default |
| --- | --- | --- |
| `DEEPSEEK_TOKEN` | API token (preferred over email/password) | — |
| `DEEPSEEK_EMAIL` | Account email (if no token) | — |
| `DEEPSEEK_PASSWORD` | Account password (if no token) | — |
| `MODEL_TYPE` | `DEFAULT`, `EXPERT`, or `VISION` | `DEFAULT` |
| `PROXY_HOST` | Bind address | `0.0.0.0` |
| `PROXY_PORT` | Port | `8000` |
| `REQUEST_DELAY` | Seconds to wait before each request (rate-limit guard) | `0` |
| `TOOL_REMINDER_INTERVAL` | Re-inject the tool-call format every N exchanges (0 = off) | `0` |
| `TOOL_BUFFER_LIMIT` | Tool-call buffer limit | `100000` |

Set either `DEEPSEEK_TOKEN` or `DEEPSEEK_EMAIL` + `DEEPSEEK_PASSWORD`.

## Run

```bash
uvicorn main:app --host 0.0.0.0 --port 8000
# or
python main.py
```

Point your OpenAI client at `http://localhost:8000/v1`.

## Tool-call wire format

The model is taught to emit tools as XML blocks:

- Opening tag carries the tool name: `<invoke name="tool_name">`.
- Each argument is its own `<parameter name="...">value