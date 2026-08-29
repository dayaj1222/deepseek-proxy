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

TOOL_REMINDER_INTERVAL = int(os.getenv("TOOL_REMINDER_INTERVAL", "0"))

TOOL_BUFFER_LIMIT = int(os.getenv("TOOL_BUFFER_LIMIT", "100000"))

_enc = tiktoken.get_encoding("cl100k_base")


def estimate_tokens(text: str) -> int:
    if not text:
        return 0
    return len(_enc.encode(text, disallowed_special=()))


TOOL_TAG_OPEN = "<invoke"
TOOL_TAG_CLOSE = "</invoke>"
TOOL_PARAM_OPEN = "<parameter"
TOOL_PARAM_CLOSE = "</parameter>"

TOOL_CALL_TEMPLATE = (
    '<invoke name="tool_name">\n'
    '<parameter name="param1">value1</parameter>\n'
    '<parameter name="param2">value2</parameter>\n'
    "</invoke>"
)
# Backwards-compatible aliases — old imports keep working
TOOL_CALL_PREFIX = TOOL_TAG_OPEN
TOOL_CALL_SUFFIX = TOOL_TAG_CLOSE
