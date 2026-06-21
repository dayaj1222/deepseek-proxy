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

TOOL_BUFFER_LIMIT = int(os.getenv("TOOL_BUFFER_LIMIT", "100000"))


_enc = tiktoken.get_encoding("cl100k_base")

def estimate_tokens(text: str) -> int:
    if not text:
        return 0
    return len(_enc.encode(text, disallowed_special=()))
