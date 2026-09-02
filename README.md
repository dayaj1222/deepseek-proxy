# DeepSeek

A FastAPI proxy that exposes an **OpenAI-compatible** `POST /v1/chat/completions` endpoint backed by the **DeepSeek web API** (via [`aiodeepseek`](https://github.com/nicepkg/aiodeepseek)).

Point any OpenAI-speaking client at it and it talks to DeepSeek — with streaming, tool calling, and per-thread conversation persistence across restarts.

## Why

The official DeepSeek API and the DeepSeek web chat are different backends. This proxy lets tools that only speak the OpenAI chat-completions schema (assistants, agents, SDKs) drive the web backend, with no protocol work on the client side.

## Features

- **OpenAI wire compatibility** — drop-in `base_url` for `openai`, `langchain`, and anything else that speaks the chat-completions schema.
- **Streaming** — server-sent events with a character-level state machine that detects tool calls *mid-stream* and re-emits them as OpenAI `tool_calls` deltas.
- **Tool calling** — a lightweight XML wire format is translated to OpenAI `tool_calls` objects, with tolerant parsing for truncated output and model drift. A literal mention of a tool-call tag inside a value is escaped, so content can never be confused with an actual call.
- **Conversation persistence** — one DeepSeek chat session per thread, reconnected after restart via `session_state.json`. Full history is held server-side by DeepSeek; only session metadata is persisted locally.
- **No database** — each request sends only the *new* messages (a delta); the proxy tracks the rest.
- **Rate-limit guard** — optional per-request delay, plus a periodic tool-format reminder that re-anchors the model's format without bloating every prompt.

## How it works

```
OpenAI client ──► FastAPI /v1/chat/completions ──► aiodeepseek ──► DeepSeek web
                     │  (thread id, prompt build,
                     │   tool-call XML ↔ JSON,
                     │   SSE streaming state machine)
                     └──► session_state.json (per-thread session / parent_message_id)
```

- **Threading** — each conversation is keyed by a `thread_id`. The client can pass one explicitly, or the proxy derives a stable id by hashing the first user message. DeepSeek keeps the conversation server-side; the proxy only reconnects to it.
- **Title-request isolation** — clients that fire auxiliary "name this session" calls get their own thread namespace, so a title request can never cross-contaminate the real chat.
- **Delta prompts** — only messages that arrived after the last assistant turn are forwarded; prior context lives in DeepSeek's session, so prompts stay small.

## Setup

```bash
git clone https://github.com/dayaj1222/deepseek.git
cd deepseek

# create and activate a virtualenv
uv venv
source .venv/bin/activate

# install dependencies
uv pip install -r requirements.txt
```

Requires Python ≥ 3.9.

## Configuration

Copy `config.example.toml` to `config.toml` and fill in your credentials. `config.toml` is the source of truth; **process environment variables override it**. There is no `.env` layer.

| Variable | Description | Default |
| --- | --- | --- |
| `DEEPSEEK_TOKEN` | Web API token (preferred) | — |
| `DEEPSEEK_EMAIL` | Account email (if no token) | — |
| `DEEPSEEK_PASSWORD` | Account password (if no token) | — |
| `MODEL_TYPE` | `DEFAULT`, `EXPERT`, or `VISION` | `DEFAULT` |
| `PROXY_HOST` | Bind address | `0.0.0.0` |
| `PROXY_PORT` | Port | `8000` |
| `REQUEST_DELAY` | Seconds to wait before each request (rate-limit guard) | `0` |
| `TOOL_REMINDER_INTERVAL` | Re-inject the tool-call format every N exchanges (`0` = off) | `0` |
| `STATE_PATH` | Where `session_state.json` lives | `./session_state.json` |
| `LOG_LEVEL` / `LOG_FORMAT` | Logging control | `INFO` / `pretty` |
| `DEBUG` | Dump full request/prompt bodies | `false` |

Set **either** `DEEPSEEK_TOKEN` **or** `DEEPSEEK_EMAIL` + `DEEPSEEK_PASSWORD`.

The `/v1/models` catalog (model ids, context lengths, token caps) is configured under `[[MODELS]]` in the same file.

## Run

```bash
uvicorn main:app --host 0.0.0.0 --port 8000
# or
python main.py
```

Point your OpenAI client at `http://localhost:8000/v1`:

```python
from openai import OpenAI

client = OpenAI(base_url="http://localhost:8000/v1", api_key="anything")
resp = client.chat.completions.create(
    model="deepseek-v4",
    messages=[{"role": "user", "content": "Hello"}],
)
print(resp.choices[0].message.content)
```

## Tool calling

Tools are declared the standard OpenAI way (`tools=[{...}]`), but the proxy teaches the model to emit calls as a lightweight XML format instead of JSON. Those blocks — whether in a final response or arriving mid-stream — are parsed and re-emitted as standard OpenAI `tool_calls` objects.

The parser is deliberately tolerant: it handles truncated calls cut off by the token limit, two legacy fallback shapes, and escaped markers inside values. A dropped tool call degrades to a logged warning, never a crash.

## Architecture

| File | Responsibility |
| --- | --- |
| `main.py` | FastAPI app, request lifecycle, prompt building, non-streaming responses |
| `streaming_handler.py` | Char-level SSE state machine that separates text from tool-call blocks in the token stream |
| `tool_parser.py` | Tool-call extraction, XML ↔ OpenAI conversion, escape masking, system-prompt injection |
| `deepseek_client.py` | Wraps `aiodeepseek` `Conversation` objects, session-state persistence, reconnection |
| `config.py` | TOML + env config, token estimation (tiktoken `cl100k_base`) |

## Notes

- This uses the **DeepSeek web API** (via `aiodeepseek`), not the official DeepSeek Platform API. Tokens, model ids, and rate limits are whatever that backend provides.
- Session state is flushed to disk atomically on shutdown and on `SIGHUP`/`SIGQUIT`; a crash mid-stream loses at most the in-flight turn's accounting.

## License

MIT
