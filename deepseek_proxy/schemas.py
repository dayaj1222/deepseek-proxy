"""Supported Chat Completions input contract."""

from typing import Any, Literal

from pydantic import BaseModel, ConfigDict, Field, model_validator


class Message(BaseModel):
    role: Literal["system", "developer", "user", "assistant", "tool"]
    content: str | list[dict[str, Any]] | None = None
    tool_calls: list[dict[str, Any]] | None = None
    tool_call_id: str | None = None
    name: str | None = None

    @model_validator(mode="after")
    def validate_message(self):
        if self.role == "tool" and not self.tool_call_id:
            raise ValueError("Tool messages require tool_call_id")
        if self.content is None and not (self.role == "assistant" and self.tool_calls):
            raise ValueError("Message requires content or assistant tool_calls")
        if isinstance(self.content, list):
            for part in self.content:
                if part.get("type") == "text" and isinstance(part.get("text"), str):
                    continue
                if self.role == "user" and part.get("type") == "image_url":
                    if isinstance(part.get("image_url"), dict) and isinstance(
                        part["image_url"].get("url"), str
                    ):
                        continue
                raise ValueError("Unsupported or invalid message content part")
        return self


class ToolFunction(BaseModel):
    name: str = Field(min_length=1, max_length=128)
    description: str | None = None
    parameters: dict[str, Any] = Field(default_factory=dict)
    strict: bool | None = None


class Tool(BaseModel):
    type: Literal["function"] = "function"
    function: ToolFunction


class NamedFunction(BaseModel):
    name: str


class NamedToolChoice(BaseModel):
    type: Literal["function"]
    function: NamedFunction


class StreamOptions(BaseModel):
    model_config = ConfigDict(extra="forbid")
    include_usage: bool = False


class ChatRequest(BaseModel):
    # OpenAI clients (including Hermes) add provider-specific metadata and
    # generation knobs.  Ignore fields this backend cannot use rather than
    # rejecting an otherwise valid chat request at the HTTP boundary.
    model_config = ConfigDict(extra="ignore")
    model: str = Field(min_length=1)
    messages: list[Message] = Field(min_length=1)
    tools: list[Tool] | None = None
    tool_choice: Literal["auto", "none", "required"] | NamedToolChoice | None = None
    parallel_tool_calls: bool = True
    stream: bool = False
    stream_options: StreamOptions | None = None
    thread_id: str | None = Field(default=None, min_length=1, max_length=256)
    user: str | None = None
    n: Literal[1] = 1
    # Recognized for clear capability errors; the backend cannot enforce them.
    temperature: float | None = None
    top_p: float | None = None
    max_tokens: int | None = Field(default=None, ge=1)
    max_completion_tokens: int | None = Field(default=None, ge=1)
    stop: str | list[str] | None = None
    response_format: dict[str, Any] | None = None
    # Official thinking params clients already send. Accepted (not rejected) and
    # used to override the THINKING_ENABLED setting per request.
    reasoning_effort: str | None = None
    reasoning: dict[str, Any] | None = None
    thinking: dict[str, Any] | None = None

    @model_validator(mode="after")
    def validate_tools(self):
        names = [t.function.name for t in self.tools or []]
        if len(names) != len(set(names)):
            raise ValueError("Tool names must be unique")
        if self.tool_choice == "required" and not names:
            raise ValueError("tool_choice=required requires tools")
        if (
            isinstance(self.tool_choice, NamedToolChoice)
            and self.tool_choice.function.name not in names
        ):
            raise ValueError("Named tool_choice must identify a supplied tool")
        if self.stream_options is not None and not self.stream:
            raise ValueError("stream_options requires stream=true")
        return self


# Values of reasoning_effort that mean "off".
_DISABLED_EFFORTS = {"", "none", "off", "disabled", "minimal"}
# Values of a thinking object's "type" that mean "on".
_ENABLED_THINKING_TYPES = {"enabled", "enabled_thinking", "on", "true", "thinking"}


def _explicit_thinking(request: ChatRequest) -> bool | None:
    """Resolve an explicit per-request thinking intent, or None if absent.

    Precedence: ``reasoning_effort`` (OpenAI-style string) >
    ``reasoning`` object (``{"effort": ...}``) > ``thinking`` object
    (``{"type": "enabled"}``).
    """
    effort = request.reasoning_effort
    if isinstance(effort, str) and effort.strip():
        return effort.strip().lower() not in _DISABLED_EFFORTS

    reasoning = request.reasoning
    if isinstance(reasoning, dict):
        inner = reasoning.get("effort", reasoning.get("enabled", reasoning.get("type")))
        if isinstance(inner, bool):
            return inner
        if isinstance(inner, str) and inner.strip():
            return inner.strip().lower() not in _DISABLED_EFFORTS

    thinking = request.thinking
    if isinstance(thinking, dict):
        inner = thinking.get("type", thinking.get("enabled"))
        if isinstance(inner, bool):
            return inner
        if isinstance(inner, str) and inner.strip():
            return inner.strip().lower() in _ENABLED_THINKING_TYPES

    return None


def want_thinking(request: ChatRequest, settings) -> bool:
    """Per-request thinking intent, falling back to the THINKING_ENABLED setting."""
    explicit = _explicit_thinking(request)
    if explicit is not None:
        return explicit
    return bool(getattr(settings, "thinking_enabled", False))
