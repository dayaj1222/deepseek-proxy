import os
from dotenv import load_dotenv

load_dotenv()

# aiodeepseek credentials
DEEPSEEK_TOKEN = os.getenv("DEEPSEEK_TOKEN")
DEEPSEEK_EMAIL = os.getenv("DEEPSEEK_EMAIL")
DEEPSEEK_PASSWORD = os.getenv("DEEPSEEK_PASSWORD")
MODEL_TYPE = os.getenv("MODEL_TYPE", "DEFAULT")

PROXY_HOST = os.getenv("PROXY_HOST", "0.0.0.0")
PROXY_PORT = int(os.getenv("PROXY_PORT", "8000"))
