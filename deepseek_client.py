import os
from typing import AsyncGenerator, Dict, Optional
import threading
from aiodeepseek import DeepSeekClient
from aiodeepseek.types.enums import ModelType
from aiodeepseek.conversation import Conversation
from config import DEEPSEEK_TOKEN, DEEPSEEK_EMAIL, DEEPSEEK_PASSWORD, MODEL_TYPE

_model_map = {
    "DEFAULT": ModelType.DEFAULT,
    "EXPERT": ModelType.EXPERT,
    "VISION": ModelType.VISION,
}

def _get_model_type() -> ModelType:
    return _model_map.get(MODEL_TYPE.upper(), ModelType.DEFAULT)

_client: Optional[DeepSeekClient] = None
_conversations: Dict[str, Conversation] = {}
_sent_counts: Dict[str, int] = {}  # thread_id -> number of messages already sent to DeepSeek
_lock = threading.Lock()

async def _ensure_client():
    global _client
    if _client is not None:
        return
    if DEEPSEEK_TOKEN:
        _client = DeepSeekClient(token=DEEPSEEK_TOKEN, model=_get_model_type())
    elif DEEPSEEK_EMAIL and DEEPSEEK_PASSWORD:
        _client = DeepSeekClient(email=DEEPSEEK_EMAIL, password=DEEPSEEK_PASSWORD, model=_get_model_type())
    else:
        raise ValueError("Either DEEPSEEK_TOKEN or (DEEPSEEK_EMAIL + DEEPSEEK_PASSWORD) must be set")
    await _client.__aenter__()

async def get_or_create_conversation(thread_id: str) -> Conversation:
    await _ensure_client()
    assert _client is not None
    with _lock:
        if thread_id not in _conversations:
            _conversations[thread_id] = _client.new_conversation()
        return _conversations[thread_id]

def get_sent_count(thread_id: str) -> int:
    with _lock:
        return _sent_counts.get(thread_id, 0)

def set_sent_count(thread_id: str, count: int):
    with _lock:
        _sent_counts[thread_id] = count

async def generate_response(thread_id: str, prompt: str, stream: bool = False):
    conv = await get_or_create_conversation(thread_id)
    if stream:
        async for chunk in conv.ask_stream(prompt):
            yield chunk
    else:
        response = await conv.ask(prompt)
        yield response.text

async def shutdown_client():
    global _client
    if _client:
        await _client.__aexit__(None, None, None)
        _client = None
