"""Thread-identity tests.

A thread id must survive a mid-conversation compaction. Clients that advertise a
session id (pi via `sendSessionAffinityHeaders`, opencode via `x-session-affinity`)
are keyed on that stable id; everything else falls back to hashing the first
user message.

Run with: .venv/bin/python -m pytest tests/test_thread_identity.py
"""

import unittest
from types import SimpleNamespace

from deepseek_proxy.prompts import get_thread_id, is_title_request
from deepseek_proxy.schemas import ChatRequest
from deepseek_proxy.transport import thread_id_from_headers


def request(messages, **kwargs):
    return ChatRequest.model_validate({"model": "DEFAULT", "messages": messages, **kwargs})


SYSTEM = {"role": "system", "content": "You are pi."}
SUMMARY = {
    "role": "user",
    "content": "The conversation history before this point was compacted into the "
    "following summary:\n\n<summary>\nWe were debugging the reset bug.\n</summary>",
}


class HeaderExtractionTests(unittest.TestCase):
    def test_reads_opencode_header(self):
        self.assertEqual(thread_id_from_headers({"x-session-affinity": "ses_abc123"}), "ses_abc123")

    def test_reads_pi_header(self):
        self.assertEqual(thread_id_from_headers({"session_id": "01a0e8b0-0744"}), "01a0e8b0-0744")

    def test_header_lookup_is_case_insensitive(self):
        self.assertEqual(thread_id_from_headers({"X-Session-Id": "sess_9"}), "sess_9")

    def test_ignores_unknown_and_blank_headers(self):
        self.assertIsNone(thread_id_from_headers({}))
        self.assertIsNone(thread_id_from_headers({"x-session-affinity": "   "}))
        self.assertIsNone(thread_id_from_headers({"user-agent": "pi/0.87.1"}))


class SessionKeyedThreadTests(unittest.TestCase):
    def test_session_id_survives_compaction(self):
        """The regression: a compaction rewrites the first user message."""
        headers = {"x-session-affinity": "01a0e8b0-0744-7379-ba61-c67093c7b3ec"}
        before = request([SYSTEM, {"role": "user", "content": "why does it reset?"}])
        after = request([SYSTEM, SUMMARY, {"role": "user", "content": "now fix it"}])

        self.assertEqual(get_thread_id(before, headers), get_thread_id(after, headers))

    def test_distinct_sessions_get_distinct_threads(self):
        a = get_thread_id(
            request([SYSTEM, {"role": "user", "content": "hi"}]),
            {"x-session-affinity": "session-a"},
        )
        b = get_thread_id(
            request([SYSTEM, {"role": "user", "content": "hi"}]),
            {"x-session-affinity": "session-b"},
        )
        self.assertNotEqual(a, b)

    def test_thread_id_is_prefixed_and_length_bounded(self):
        tid = get_thread_id(
            request([SYSTEM, {"role": "user", "content": "hi"}]),
            {"x-session-affinity": "s" * 500},
        )
        self.assertTrue(tid.startswith("thread_"))
        self.assertLessEqual(len(tid), 40)

    def test_title_requests_never_share_the_session_thread(self):
        """A title-gen call inherits the parent's session headers.

        Both must still land in different namespaces or their concurrent
        responses cross (see is_title_request).
        """
        headers = {"x-session-affinity": "session-a"}
        chat = request([SYSTEM, {"role": "user", "content": "hello"}])
        title = request(
            [
                {"role": "system", "content": "You name chat sessions"},
                {"role": "user", "content": "hello"},
            ]
        )
        self.assertTrue(is_title_request(title))
        self.assertNotEqual(get_thread_id(chat, headers), get_thread_id(title, headers))
        self.assertTrue(get_thread_id(title, headers).startswith("title_"))


class FirstMessageFallbackTests(unittest.TestCase):
    def test_fallback_used_when_no_header(self):
        req = request([SYSTEM, {"role": "user", "content": "why does it reset?"}])
        self.assertTrue(get_thread_id(req, {}).startswith("thread_"))

    def test_fallback_is_stable_across_turns(self):
        headers = {}
        first = request([SYSTEM, {"role": "user", "content": "start"}])
        later = request(
            [
                SYSTEM,
                {"role": "user", "content": "start"},
                {"role": "assistant", "content": "ok"},
                {"role": "user", "content": "next"},
            ]
        )
        self.assertEqual(get_thread_id(first, headers), get_thread_id(later, headers))

    def test_fallback_breaks_on_compaction(self):
        """Documents the known limitation the session header exists to fix."""
        headers = {}
        before = request([SYSTEM, {"role": "user", "content": "why does it reset?"}])
        after = request([SYSTEM, SUMMARY, {"role": "user", "content": "now fix it"}])
        self.assertNotEqual(get_thread_id(before, headers), get_thread_id(after, headers))

    def test_explicit_body_thread_id_wins_over_everything(self):
        req = request([SYSTEM, {"role": "user", "content": "hi"}], thread_id="caller-supplied")
        self.assertEqual(get_thread_id(req, {"x-session-affinity": "session-a"}), "caller-supplied")


class RouteHeaderTests(unittest.TestCase):
    """The header must survive the real HTTP boundary, not just the helper."""

    def test_completions_route_keys_on_session_header(self):
        from fastapi.testclient import TestClient

        from deepseek_proxy.app import create_app
        from deepseek_proxy.errors import ProxyError

        seen = []

        class FakeService:
            settings = SimpleNamespace(heartbeat_interval=10, request_timeout=30)

            def validate(self, request):
                pass

            async def events(self, request, headers=None):
                from deepseek_proxy.prompts import get_thread_id

                seen.append(get_thread_id(request, headers))
                raise ProxyError("stop here", 400, "probe")
                yield  # pragma: no cover

        app = create_app(settings=SimpleNamespace(), pool=object())
        app.state.service = FakeService()
        client = TestClient(app, raise_server_exceptions=False)

        body = {"model": "DEFAULT", "messages": [{"role": "user", "content": "hi"}]}
        client.post("/v1/chat/completions", json=body, headers={"x-session-affinity": "ses_e2e"})
        self.assertTrue(seen)
        # Same session, different first message (as compaction produces).
        body["messages"] = [{"role": "user", "content": "compacted summary text"}]
        client.post("/v1/chat/completions", json=body, headers={"x-session-affinity": "ses_e2e"})
        self.assertEqual(seen[0], seen[1])

        # A different session must not collide with the first.
        client.post("/v1/chat/completions", json=body, headers={"x-session-affinity": "ses_other"})
        self.assertNotEqual(seen[1], seen[2])


if __name__ == "__main__":
    unittest.main()
