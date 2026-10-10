"""Unit tests for thinking/search mode: settings, param mapping, SSE routing.

All SSE payloads are synthetic — no network calls are made.
"""

import importlib.util
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
PACKAGE = ROOT / "deepseek_proxy"


def load_settings(path, env=None):
    spec = importlib.util.spec_from_file_location(
        "deepseek_proxy.settings", PACKAGE / "settings.py"
    )
    module = importlib.util.module_from_spec(spec)
    with patch.dict(os.environ, {"DEEPSEEK_CONFIG": str(path), **(env or {})}, clear=True):
        with patch.dict(sys.modules, {spec.name: module}):
            spec.loader.exec_module(module)
    return module


def load_thinking():
    spec = importlib.util.spec_from_file_location(
        "deepseek_proxy.thinking", PACKAGE / "thinking.py"
    )
    module = importlib.util.module_from_spec(spec)
    with patch.dict(sys.modules, {spec.name: module}):
        spec.loader.exec_module(module)
    return module


def load_schemas():
    # schemas imports nothing from the package, so load it directly.
    spec = importlib.util.spec_from_file_location("deepseek_proxy.schemas", PACKAGE / "schemas.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class SettingDefaultsTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.path = Path(self.temp.name) / "config.toml"

    def test_both_default_false(self):
        config = load_settings(self.path)
        self.assertFalse(config.settings.thinking_enabled)
        self.assertTrue(config.settings.search_enabled)
        self.assertFalse(config.THINKING_ENABLED)
        self.assertTrue(config.SEARCH_ENABLED)

    def test_enabled_from_toml(self):
        self.path.write_text("THINKING_ENABLED = true\nSEARCH_ENABLED = true\n")
        config = load_settings(self.path)
        self.assertTrue(config.settings.thinking_enabled)
        self.assertTrue(config.settings.search_enabled)

    def test_env_overrides_toml(self):
        self.path.write_text("THINKING_ENABLED = true\n")
        config = load_settings(self.path, {"THINKING_ENABLED": "false"})
        self.assertFalse(config.settings.thinking_enabled)

    def test_truthy_and_falsy_spellings(self):
        for value in ("1", "true", "yes", "on"):
            with self.subTest(value=value):
                config = load_settings(self.path, {"SEARCH_ENABLED": value})
                self.assertTrue(config.settings.search_enabled)
        for value in ("0", "false", "no", "off", ""):
            with self.subTest(value=value):
                config = load_settings(self.path, {"SEARCH_ENABLED": value})
                self.assertFalse(config.settings.search_enabled)


class WantThinkingMatrixTests(unittest.TestCase):
    """reasoning_effort / thinking / reasoning present/absent x setting on/off."""

    def setUp(self):
        self.schemas = load_schemas()
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.path = Path(self.temp.name) / "config.toml"

    def _request(self, **extra):
        base = {
            "model": "deepseek-chat",
            "messages": [{"role": "user", "content": "hi"}],
        }
        base.update(extra)
        return self.schemas.ChatRequest(**base)

    def _setting(self, enabled):
        config = load_settings(self.path, {"THINKING_ENABLED": "true" if enabled else "false"})
        return config.settings

    def test_absent_falls_back_to_setting(self):
        req = self._request()
        self.assertTrue(self.schemas.want_thinking(req, self._setting(True)))
        self.assertFalse(self.schemas.want_thinking(req, self._setting(False)))

    def test_reasoning_effort_overrides_setting(self):
        high = self._request(reasoning_effort="high")
        none = self._request(reasoning_effort="none")
        self.assertTrue(self.schemas.want_thinking(high, self._setting(False)))
        self.assertFalse(self.schemas.want_thinking(none, self._setting(True)))

    def test_reasoning_object_effort(self):
        on = self._request(reasoning={"effort": "medium"})
        off = self._request(reasoning={"effort": "minimal"})
        self.assertTrue(self.schemas.want_thinking(on, self._setting(False)))
        self.assertFalse(self.schemas.want_thinking(off, self._setting(True)))

    def test_thinking_object_type(self):
        on = self._request(thinking={"type": "enabled"})
        off = self._request(thinking={"type": "disabled"})
        self.assertTrue(self.schemas.want_thinking(on, self._setting(False)))
        self.assertFalse(self.schemas.want_thinking(off, self._setting(True)))

    def test_params_are_accepted_not_rejected(self):
        req = self._request(
            reasoning_effort="high",
            reasoning={"effort": "low"},
            thinking={"type": "enabled"},
        )
        self.assertEqual(req.reasoning_effort, "high")
        self.assertEqual(req.reasoning, {"effort": "low"})
        self.assertEqual(req.thinking, {"type": "enabled"})


def _route_all(router, events):
    """Feed events through thinking._route_event, collecting channels."""
    out = []
    for event in events:
        for item in thinking_route(router, event):
            out.append(item)
    return out


def thinking_route(router, event):
    from deepseek_proxy import thinking

    return list(thinking._route_event(event, router))


class FragmentRouterTests(unittest.TestCase):
    def setUp(self):
        self.thinking = load_thinking()
        # Ensure deepseek_proxy.thinking is importable for the helper.

    def _route(self, events):
        router = self.thinking._FragmentRouter()
        text, reasoning = [], []
        for event in events:
            for channel, piece in self.thinking._route_event(event, router):
                if channel == "reasoning":
                    reasoning.append(piece)
                elif channel == "text":
                    text.append(piece)
        return "".join(text), "".join(reasoning)

    def test_think_then_response_exact_shapes(self):
        # EXACT documented shapes: initial bulk declares fragments; a BATCH
        # APPEND-list declares a NEW fragment; content chunks belong to the
        # last declared fragment.
        events = [
            # Initial bulk object declares THINK then RESPONSE.
            {
                "o": "BATCH",
                "v": {
                    "response": {
                        "fragments": [
                            {"id": 1, "type": "THINK", "content": ""},
                            {"id": 2, "type": "RESPONSE", "content": ""},
                        ]
                    }
                },
            },
            # Chunks for the current (last) fragment: RESPONSE.
            {"v": "Hello"},
            {"o": "APPEND", "p": "response/fragments/-1/content", "v": " world"},
            # BATCH APPEND-list declares a NEW THINK fragment -> current becomes THINK.
            {"o": "APPEND", "p": "response/fragments", "v": [{"id": 3, "type": "THINK"}]},
            {"v": "secret reasoning"},
            {"o": "APPEND", "p": "response/fragments/-1/content", "v": " more secret"},
            # New RESPONSE fragment.
            {"o": "APPEND", "p": "response/fragments", "v": [{"id": 4, "type": "RESPONSE"}]},
            {"v": "Final answer [citation:1]"},
        ]
        text, reasoning = self._route(events)
        self.assertEqual(text, "Hello worldFinal answer [citation:1]")
        self.assertEqual(reasoning, "secret reasoning more secret")

    def test_thinking_text_never_reaches_text_channel(self):
        events = [
            {"v": {"response": {"fragments": [{"id": 1, "type": "THINK"}]}}},
            {"v": "chain of thought"},
        ]
        text, reasoning = self._route(events)
        self.assertEqual(text, "")
        self.assertEqual(reasoning, "chain of thought")

    def test_search_results_dropped(self):
        events = [
            {"v": {"response": {"fragments": [{"id": 1, "type": "SEARCH"}]}}},
            {"v": "search query text"},
            {
                "p": "response/fragments/-1/results",
                "v": [{"url": "http://x", "title": "t", "cite_index": 1}],
            },
        ]
        text, reasoning = self._route(events)
        self.assertEqual(text, "")
        self.assertEqual(reasoning, "")

    def test_status_metadata_never_becomes_reply_text(self):
        text, reasoning = self._route(
            [
                {"o": "SET", "p": "response/status", "v": "FINISHED"},
                {
                    "o": "BATCH",
                    "p": "response",
                    "v": [
                        {"p": "quasi_status", "v": "FINISHED"},
                    ],
                },
            ]
        )
        self.assertEqual(text, "")
        self.assertEqual(reasoning, "")

    def test_think_transition_keeps_first_answer_token_and_shorthand_delta(self):
        text, reasoning = self._route(
            [
                {"v": {"response": {"fragments": [{"type": "THINK", "content": "Reason"}]}}},
                {"p": "response/fragments/-1/content", "o": "APPEND", "v": " first"},
                {"p": "response/fragments/-1/elapsed_secs", "o": "SET", "v": 1.0},
                {
                    "p": "response/fragments",
                    "o": "APPEND",
                    "v": [
                        {"type": "RESPONSE", "content": "Hello"},
                    ],
                },
                {"p": "response/fragments/-1/content", "v": " world"},
                {"v": "!"},
                {"p": "response/status", "o": "SET", "v": "FINISHED"},
            ]
        )
        self.assertEqual(reasoning, "Reason first")
        self.assertEqual(text, "Hello world!")

    def test_search_batch_declares_response_before_answer_deltas(self):
        text, reasoning = self._route(
            [
                {"v": {"response": {"fragments": [{"type": "SEARCH", "content": None}]}}},
                {
                    "p": "response/fragments/-1",
                    "o": "BATCH",
                    "v": [
                        {"p": "status", "v": "FINISHED"},
                        {"p": "content", "v": "Found 12 pages"},
                    ],
                },
                {
                    "p": "response",
                    "o": "BATCH",
                    "v": [
                        {
                            "p": "fragments",
                            "o": "APPEND",
                            "v": [
                                {"type": "RESPONSE", "content": "Answer"},
                            ],
                        },
                        {"p": "has_pending_fragment", "o": "SET", "v": False},
                    ],
                },
                {"p": "response/fragments/-1/content", "o": "APPEND", "v": " [citation:1]"},
                {"p": "response/status", "o": "SET", "v": "FINISHED"},
            ]
        )
        self.assertEqual(text, "Answer [citation:1]")
        self.assertEqual(reasoning, "")

    def test_search_between_thinking_stages_does_not_open_answer_channel(self):
        router = self.thinking._FragmentRouter()
        events = [
            {"v": {"response": {"fragments": [{"type": "THINK", "content": "plan"}]}}},
            {
                "p": "response",
                "o": "BATCH",
                "v": [
                    {"p": "fragments", "o": "APPEND", "v": [{"type": "SEARCH"}]},
                ],
            },
            {
                "p": "response/fragments/-1",
                "o": "BATCH",
                "v": [
                    {"p": "content", "v": "Found 10 web pages"},
                    {"p": "status", "v": "FINISHED"},
                ],
            },
            {
                "p": "response/fragments",
                "o": "APPEND",
                "v": [
                    {"type": "THINK", "content": "evaluate"},
                ],
            },
            {
                "p": "response/fragments",
                "o": "APPEND",
                "v": [
                    {"type": "RESPONSE", "content": "answer"},
                ],
            },
        ]
        pieces = [piece for event in events for piece in self.thinking._route_event(event, router)]
        self.assertEqual(
            pieces,
            [
                ("reasoning", "plan"),
                ("reasoning", "evaluate"),
                ("text", "answer"),
            ],
        )

    def test_citation_markers_preserved(self):
        events = [
            {"v": {"response": {"fragments": [{"id": 1, "type": "RESPONSE"}]}}},
            {"v": "See [citation:1] and [citation:2]."},
        ]
        text, _ = self._route(events)
        self.assertEqual(text, "See [citation:1] and [citation:2].")

    def test_chunks_before_any_declaration_go_to_text(self):
        events = [{"v": "orphan text"}]
        text, reasoning = self._route(events)
        self.assertEqual(text, "orphan text")
        self.assertEqual(reasoning, "")


class ReasoningCaptureTests(unittest.TestCase):
    def setUp(self):
        self.thinking = load_thinking()

    def test_capture_collects_and_isolates(self):
        with self.thinking.reasoning_capture() as sink:
            self.thinking._append_reasoning("a")
            self.thinking._append_reasoning("b")
        self.assertEqual(sink, ["a", "b"])
        self.thinking._append_reasoning("c")
        self.assertEqual(sink, ["a", "b"])

    def test_capture_notifies_for_each_reasoning_fragment(self):
        observed = []
        with self.thinking.reasoning_capture(on_piece=observed.append) as sink:
            self.thinking._append_reasoning("first")
            self.thinking._append_reasoning("second")
        self.assertEqual(observed, ["first", "second"])
        self.assertEqual(sink, ["first", "second"])

    def test_request_flags_are_scoped(self):
        with self.thinking.request_flags(True, False):
            self.assertTrue(self.thinking._thinking_on.get())
            self.assertFalse(self.thinking._search_on.get())
        self.assertFalse(self.thinking._thinking_on.get())


class ApplyTests(unittest.TestCase):
    def test_apply_false_still_installs_router_for_per_request_opt_in(self):
        from aiodeepseek.clients import chat as chat_client

        thinking = load_thinking()
        original = chat_client._ChatClient.stream_chat
        try:
            self.assertTrue(thinking.apply(False))
            self.assertIs(chat_client._ChatClient.stream_chat, thinking._routed_stream_chat)
        finally:
            chat_client._ChatClient.stream_chat = original

    def test_apply_true_installs_routed_stream_chat(self):
        from aiodeepseek.clients import chat as chat_client

        original = chat_client._ChatClient.stream_chat
        thinking = load_thinking()
        try:
            self.assertTrue(thinking.apply(True))
            self.assertIs(chat_client._ChatClient.stream_chat, thinking._routed_stream_chat)
        finally:
            chat_client._ChatClient.stream_chat = original


class RoutedStreamChatIntegrationTests(unittest.TestCase):
    """Drive the patched stream_chat over a synthetic SSE byte stream."""

    def setUp(self):
        self.thinking = load_thinking()

    def _run(self, body_lines, thinking_on, search_on):
        import asyncio

        captured = {}

        class FakeResponse:
            status = 200

            def __init__(self, lines):
                self._lines = lines

            async def __aenter__(self):
                return self

            async def __aexit__(self, *exc):
                return False

            @property
            def content(self):
                async def gen():
                    for line in self._lines:
                        yield (line + "\n").encode("utf-8")

                return gen()

            async def text(self):
                return ""

        class FakeSession:
            def post(self, *args, **kwargs):
                captured["body"] = kwargs.get("json")
                return FakeResponse(body_lines)

        class FakeClient:
            _session = FakeSession()

            async def _build_pow_header(self, *a, **k):
                return "pow"

            def _effective_timeout(self, t):
                return t

            def _aiohttp_timeout(self, t):
                return None

        client = FakeClient()

        async def drain():
            text, reasoning = [], []
            with self.thinking.request_flags(thinking_on, search_on):
                with self.thinking.reasoning_capture() as sink:
                    async for piece, _mid in self.thinking._routed_stream_chat(
                        client, "tok", "sess", "hi", None, cumulative=False
                    ):
                        text.append(piece)
            reasoning.extend(sink)
            return "".join(text), "".join(reasoning)

        text, reasoning = asyncio.run(drain())
        return text, reasoning, captured["body"]

    def test_flags_land_in_body_and_think_is_routed(self):
        lines = [
            "data: " + '{"v": {"response": {"fragments": '
            '[{"id": 1, "type": "THINK"}, {"id": 2, "type": "RESPONSE"}]}}}',
            "data: " + '{"v": "Hello"}',
            "data: " + '{"o": "APPEND", "p": "response/fragments", '
            '"v": [{"id": 3, "type": "THINK"}]}',
            "data: " + '{"v": "secret thinking"}',
            "data: " + '{"o": "APPEND", "p": "response/fragments", '
            '"v": [{"id": 4, "type": "RESPONSE"}]}',
            "data: " + '{"v": " world"}',
            "data: [DONE]",
        ]
        text, reasoning, body = self._run(lines, thinking_on=True, search_on=True)
        self.assertTrue(body["thinking_enabled"])
        self.assertTrue(body["search_enabled"])
        self.assertEqual(text, "Hello world")
        self.assertEqual(reasoning, "secret thinking")

    def test_flags_off_sends_false(self):
        lines = ["data: " + '{"v": "plain"}', "data: [DONE]"]
        text, reasoning, body = self._run(lines, thinking_on=False, search_on=False)
        self.assertFalse(body["thinking_enabled"])
        self.assertFalse(body["search_enabled"])
        self.assertEqual(text, "plain")
        self.assertEqual(reasoning, "")


class TransportReasoningTests(unittest.TestCase):
    def setUp(self):
        spec = importlib.util.spec_from_file_location(
            "deepseek_proxy.transport", PACKAGE / "transport.py"
        )
        self.transport = importlib.util.module_from_spec(spec)
        sys.modules[spec.name] = self.transport
        spec.loader.exec_module(self.transport)

    def _event(self, kind, value):
        from deepseek_proxy.tools.parser import Event

        return Event(kind, value)

    def test_streaming_reasoning_delta_shape(self):
        completion = self.transport.Completion("m")
        chunk = completion.accept(self._event("reasoning", "why"))
        self.assertEqual(chunk["choices"][0]["delta"], {"reasoning_content": "why"})

    def test_non_stream_omits_reasoning_key_when_absent(self):
        completion = self.transport.Completion("m")
        completion.accept(self._event("text", "answer"))
        completion.accept(self._event("done", {"prompt_tokens": 1, "finish_reason": "stop"}))
        message = completion.response()["choices"][0]["message"]
        self.assertNotIn("reasoning_content", message)

    def test_non_stream_includes_reasoning_key_when_present(self):
        completion = self.transport.Completion("m")
        completion.accept(self._event("reasoning", "why"))
        completion.accept(self._event("text", "answer"))
        completion.accept(self._event("done", {"prompt_tokens": 1, "finish_reason": "stop"}))
        message = completion.response()["choices"][0]["message"]
        self.assertEqual(message["reasoning_content"], "why")
        self.assertEqual(message["content"], "answer")

    def test_reasoning_not_counted_in_completion_tokens(self):
        completion = self.transport.Completion("m")
        completion.accept(self._event("reasoning", "x" * 1000))
        completion.accept(self._event("text", "answer"))
        completion.accept(self._event("done", {"prompt_tokens": 1, "finish_reason": "stop"}))
        usage = completion.usage()
        # Only "answer" contributes; the 1000-char reasoning does not.
        self.assertLess(usage["completion_tokens"], 20)


class LiveReasoningOrderTests(unittest.IsolatedAsyncioTestCase):
    async def test_transport_does_not_open_answer_channel_before_thinking(self):
        import json
        from types import SimpleNamespace
        from deepseek_proxy.transport import stream
        from deepseek_proxy.tools.parser import Event

        class Service:
            settings = SimpleNamespace(heartbeat_interval=1)

            async def events(self, *args):
                yield Event("reasoning", "thought")
                yield Event("text", "answer")
                yield Event("done", {"prompt_tokens": 1, "finish_reason": "stop"})

        request = SimpleNamespace(model="DEFAULT", stream_options=None)
        chunks = [part async for part in stream(Service(), request)]
        deltas = [
            json.loads(part[6:])["choices"][0]["delta"]
            for part in chunks
            if part.startswith("data: {")
        ]
        self.assertEqual(
            deltas[:3],
            [
                {"role": "assistant"},
                {"reasoning_content": "thought"},
                {"content": "answer"},
            ],
        )

    async def test_backend_forwards_thinking_before_answer_is_available(self):
        import asyncio
        from types import SimpleNamespace
        from deepseek_proxy import backend, thinking

        allow_answer = asyncio.Event()

        class Conversation:
            parent_message_id = None

            async def ask_stream(self, *args, **kwargs):
                pending = thinking._append_reasoning("first thought")
                if pending is not None:
                    await pending
                await allow_answer.wait()
                yield "answer"

        async def ensure_client():
            pass

        conn = SimpleNamespace(
            _ensure_client=ensure_client,
            client=SimpleNamespace(_session_id="saved"),
            thread_sessions={"test": "session"},
        )
        pool = object.__new__(backend.ConnectionPool)
        pool._get_or_create_conversation = lambda *_: Conversation()
        source = pool._generate_once(conn, "test", "hi", stream=True, thinking=True)
        try:
            first = await asyncio.wait_for(anext(source), 1)
            self.assertIsInstance(first, thinking.Reasoning)
            self.assertEqual(first.text, "first thought")
            self.assertFalse(allow_answer.is_set())
            allow_answer.set()
            self.assertEqual(await anext(source), "answer")
            with self.assertRaises(StopAsyncIteration):
                await anext(source)
        finally:
            await source.aclose()
        self.assertEqual(conn.client._session_id, "saved")

    async def test_stream_bridge_cancels_upstream_when_client_disconnects(self):
        import asyncio
        from deepseek_proxy import thinking

        stopped = asyncio.Event()

        async def source():
            try:
                pending = thinking._append_reasoning("thought")
                if pending is not None:
                    await pending
                await asyncio.Event().wait()
                yield "unreachable"
            finally:
                stopped.set()

        stream = thinking.stream_reasoning(source(), thinking=True, search=True)
        first = await asyncio.wait_for(anext(stream), 1)
        self.assertEqual(first.text, "thought")
        await stream.aclose()
        self.assertTrue(stopped.is_set())


if __name__ == "__main__":
    unittest.main()
