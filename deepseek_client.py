import json
import logging
from pathlib import Path
from typing import AsyncGenerator, Dict, Optional
import threading
from aiodeepseek import DeepSeekClient
from aiodeepseek.types.enums import ModelType
from aiodeepseek.types.exceptions import DeepSeekError
from aiodeepseek.conversation import Conversation
from config import DEEPSEEK_TOKEN, DEEPSEEK_EMAIL, DEEPSEEK_PASSWORD, MODEL_TYPE

log = logging.getLogger(__name__)

_model_map = {
    "DEFAULT": ModelType.DEFAULT,
    "EXPERT": ModelType.EXPERT,
    "VISION": ModelType.VISION,
}

_model_id_map = {
    "deepseek-v4-pro": ModelType.EXPERT,
    "deepseek-v4-flash": ModelType.DEFAULT,
    "deepseek-chat": ModelType.DEFAULT,
    "deepseek-reasoner": ModelType.EXPERT,
}

def _get_model_type() -> ModelType:
    return _model_map.get(MODEL_TYPE.upper(), ModelType.DEFAULT)

def _resolve_model(model_id: str) -> ModelType:
    lower = model_id.lower()
    if "vision" in lower:
        return ModelType.VISION
    return _model_id_map.get(lower, _get_model_type())

_client: Optional[DeepSeekClient] = None
_conversations: Dict[str, Conversation] = {}
_thread_sessions: Dict[str, str] = {}
_lock = threading.Lock()

STATE_PATH = Path(__file__).parent / "session_state.json"

def _load_state() -> dict:
    try:
        data = json.loads(STATE_PATH.read_text())
        data.setdefault("threads", {})
        return data
    except (FileNotFoundError, json.JSONDecodeError):
        return {"threads": {}}

def _save_state(thread_id: str, session_id: str, parent_message_id: str):
    state = _load_state()
    state.setdefault("threads", {})
    state["threads"][thread_id] = {
        "session_id": session_id,
        "parent_message_id": parent_message_id,
    }
    STATE_PATH.parent.mkdir(parents=True, exist_ok=True)
    tmp = STATE_PATH.with_suffix(".tmp")
    with open(tmp, "w") as f:
        json.dump(state, f, indent=2)
    tmp.replace(STATE_PATH)

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
    try:
        await _client.__aenter__()
    except Exception:
        _client = None
        raise

async def get_or_create_conversation(thread_id: str) -> Conversation:
    await _ensure_client()
    assert _client is not None
    with _lock:
        if thread_id not in _conversations:
            conv = _client.new_conversation()
            _conversations[thread_id] = conv
            state = _load_state()
            saved = state.get("threads", {}).get(thread_id)
            if saved:
                sid = saved.get("session_id")
                pid = saved.get("parent_message_id")
                if sid:
                    _thread_sessions[thread_id] = sid
                if pid:
                    conv._parent_message_id = pid
        return _conversations[thread_id]

async def generate_response(thread_id: str, prompt: str, model: str = "", stream: bool = False):
    conv = await get_or_create_conversation(thread_id)
    model_type = _resolve_model(model) if model else None

    session_id = _thread_sessions.get(thread_id)
    if session_id is None:
        session_id = await _client.create_chat_session(_client._token)
        with _lock:
            _thread_sessions[thread_id] = session_id

    saved_client_session = _client._session_id
    _client._session_id = session_id

    try:
        if stream:
            async for chunk in conv.ask_stream(prompt, model=model_type):
                yield chunk
        else:
            response = await conv.ask(prompt, model=model_type)
            yield response.text
    except DeepSeekError as e:
        log.error("DeepSeek API error [%s]: %s (session=%s thread=%s)",
                  type(e).__name__, repr(e), session_id, thread_id)
        if "input_exceeds_limit" in str(e).lower():
            session_id = await _client.create_chat_session(_client._token)
            with _lock:
                _thread_sessions[thread_id] = session_id
                conv = _client.new_conversation()
                _conversations[thread_id] = conv
            _client._session_id = session_id
            if stream:
                async for chunk in conv.ask_stream(prompt, model=model_type):
                    yield chunk
            else:
                response = await conv.ask(prompt, model=model_type)
                yield response.text
            with _lock:
                if conv.parent_message_id:
                    _save_state(thread_id, session_id, conv.parent_message_id)
            return
        raise
    finally:
        if _client and saved_client_session is not None:
            _client._session_id = saved_client_session

    with _lock:
        if conv.parent_message_id:
            _save_state(thread_id, session_id, conv.parent_message_id)

async def shutdown_client():
    global _client
    if _client:
        await _client.__aexit__(None, None, None)
        _client = None
