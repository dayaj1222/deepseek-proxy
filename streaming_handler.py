"""Compatibility import for the OpenAI streaming transport."""

from deepseek_proxy.transport import sse as format_sse, stream

__all__ = ["format_sse", "stream"]
