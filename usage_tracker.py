import threading
from token_counter import count_tokens

_lock = threading.Lock()
_prompt_usage = {}

def update_usage(thread_id: str, messages, response_text: str):
    # Simple token count (just response for now)
    completion_tokens = count_tokens(response_text)
    prompt_tokens = count_tokens(str(messages))  # rough estimate
    
    with _lock:
        _prompt_usage[thread_id] = _prompt_usage.get(thread_id, 0) + prompt_tokens
    
    return {
        "prompt_tokens": prompt_tokens,
        "completion_tokens": completion_tokens,
        "total_tokens": prompt_tokens + completion_tokens,
    }
