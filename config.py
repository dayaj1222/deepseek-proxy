import os

import tiktoken
from dotenv import load_dotenv

load_dotenv()

# aiodeepseek credentials
DEEPSEEK_TOKEN = os.getenv("DEEPSEEK_TOKEN")
DEEPSEEK_EMAIL = os.getenv("DEEPSEEK_EMAIL")
DEEPSEEK_PASSWORD = os.getenv("DEEPSEEK_PASSWORD")
MODEL_TYPE = os.getenv("MODEL_TYPE", "DEFAULT")

PROXY_HOST = os.getenv("PROXY_HOST", "0.0.0.0")
PROXY_PORT = int(os.getenv("PROXY_PORT", "8000"))

REQUEST_DELAY = float(os.getenv("REQUEST_DELAY", "0"))

# Every N user/tool exchanges, inject a HARD tool-format warning into the
# prompt (0 = disabled). Guards against the model drifting off the marker
# format over long agent sessions. `.env` key: TOOL_REMINDER_INTERVAL
TOOL_REMINDER_INTERVAL = int(os.getenv("TOOL_REMINDER_INTERVAL", "0"))

# Max bytes to buffer waiting for a complete tool call before falling back
# to plain text. `.env` key: TOOL_BUFFER_LIMIT
TOOL_BUFFER_LIMIT = int(os.getenv("TOOL_BUFFER_LIMIT", "100000"))

# Central definition of the tool-call marker used between the LLM and the
# proxy. streaming_handler and tool_parser both import these so the format
# is defined in exactly one place.

_enc = tiktoken.get_encoding("cl100k_base")


def estimate_tokens(text: str) -> int:
    if not text:
        return 0
    return len(_enc.encode(text, disallowed_special=()))


# ---- Tool-call wire format (single source of truth) ----
TOOL_TAG_OPEN = "<invoke"
TOOL_TAG_CLOSE = "</invoke>"
TOOL_CALL_TEMPLATE = '<invoke name="tool_name">{"param": "value"}</invoke>'
# Backwards-compatible aliases — old imports keep working
TOOL_CALL_PREFIX = TOOL_TAG_OPEN
TOOL_CALL_SUFFIX = TOOL_TAG_CLOSE
