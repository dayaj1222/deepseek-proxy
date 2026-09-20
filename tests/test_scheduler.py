"""Scheduler tests; no credentials or network calls are used.

Run with: .venv/bin/python -m unittest discover -s tests -p test_scheduler.py
"""

import asyncio
from contextlib import aclosing
import tempfile
import time
from types import SimpleNamespace
import unittest
from unittest.mock import patch

import connections as scheduler
from db import StateStore


class FakeError(scheduler.DeepSeekError):
    def __init__(self, message):
        Exception.__init__(self, message)


class FakeConversation:
    def __init__(self, client):
        self.client = client
        self._parent_message_id = None

    @property
    def parent_message_id(self):
        return self._parent_message_id

    async def ask(self, prompt, **kwargs):
        chunks = [chunk async for chunk in self.ask_stream(prompt, **kwargs)]
        return SimpleNamespace(text="".join(chunks))

    async def ask_stream(self, prompt, **kwargs):
        client = self.client
        client.events.append(("start", prompt, time.monotonic(), client._session_id))
        client.entered.set()
        try:
            failure = client.failures.pop(prompt, None)
            if failure:
                raise FakeError(failure)
            await client.proceed.wait()
            self._parent_message_id = "parent:" + prompt
            yield prompt
            await asyncio.sleep(0)
        finally:
            client.events.append(("end", prompt, time.monotonic(), client._session_id))


class FakeClient:
    instances = []
    events = []
    failures = {}

    def __init__(self, **kwargs):
        self._session_id = None
        self._token = "fake"
        self.entered = asyncio.Event()
        self.proceed = asyncio.Event()
        self.proceed.set()
        self.closed = False
        self.sessions = 0
        self.instances.append(self)

    async def __aenter__(self):
        await asyncio.sleep(0)
        return self

    async def __aexit__(self, *args):
        self.closed = True

    async def create_chat_session(self, token):
        self.sessions += 1
        await asyncio.sleep(0)
        self._session_id = "session:" + str(self.sessions)
        return self._session_id

    def new_conversation(self):
        return FakeConversation(self)


async def collect(pool, thread, prompt=None, **kwargs):
    async with aclosing(pool.generate_response(thread, prompt or thread, **kwargs)) as response:
        return [chunk async for chunk in response]


class SchedulerTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        FakeClient.instances = []
        FakeClient.events = []
        FakeClient.failures = {}
        self.patch_client = patch.object(scheduler, "DeepSeekClient", FakeClient)
        self.patch_client.start()
        self.addCleanup(self.patch_client.stop)
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.store = StateStore(self.temp.name + "/state.db")
        self.store.open()
        self.addCleanup(self.store.close)
        self.pool = scheduler.ConnectionPool(
            [{"email": "a", "password": "fake"}],
            self.store,
            request_delay=0,
        )
        self.conn = self.pool._conns[0]

    async def test_zero_delay_serializes_initialization_sessions_and_output(self):
        self.assertEqual(
            await asyncio.gather(collect(self.pool, "one"), collect(self.pool, "two")),
            [["one"], ["two"]],
        )
        self.assertEqual(len(FakeClient.instances), 1)
        events = FakeClient.events
        self.assertEqual(
            [event[:2] for event in events],
            [("start", "one"), ("end", "one"), ("start", "two"), ("end", "two")],
        )
        self.assertEqual(events[0][3], events[1][3])
        self.assertEqual(events[2][3], events[3][3])
        self.assertNotEqual(events[0][3], events[2][3])
        self.assertIsNone(self.conn.client._session_id)

    async def test_default_and_completion_to_start_gap(self):
        default = scheduler.ConnectionPool([{"email": "a", "password": "fake"}], self.store)
        self.assertGreaterEqual(default._conns[0].request_delay, 2)
        self.conn.request_delay = 0.035
        await asyncio.gather(collect(self.pool, "one"), collect(self.pool, "two"))
        self.assertGreaterEqual(FakeClient.events[2][2] - FakeClient.events[1][2], 0.03)

    async def test_turn_spans_repairs_and_generate_response_is_reentrant(self):
        async with self.pool.turn("one") as turn:
            self.assertEqual(await collect(self.pool, "one", "initial"), ["initial"])
            competitor = asyncio.create_task(collect(self.pool, "one", "competitor"))
            await asyncio.sleep(0.01)
            self.assertFalse(competitor.done())
            self.assertEqual(
                [chunk async for chunk in turn.generate("repair", stream=True)], ["repair"]
            )
        await competitor
        self.assertEqual(
            [event[1] for event in FakeClient.events if event[0] == "start"],
            ["initial", "repair", "competitor"],
        )
        with self.assertRaises(RuntimeError):
            await anext(self.pool.generate_in_turn("one", "outside"))

    async def test_queue_bound_timeout_and_cancelled_waiter(self):
        self.conn.queue_limit = 1
        self.conn.queue_timeout = 0.03
        await self.conn.acquire_rate_slot()
        waiter = asyncio.create_task(collect(self.pool, "one"))
        await asyncio.sleep(0)
        with self.assertRaises(scheduler.AccountQueueFull):
            await collect(self.pool, "two")
        with self.assertRaises(scheduler.AccountQueueTimeout):
            await waiter
        self.assertEqual(self.conn.waiting, 0)
        self.assertTrue(self.conn.rate_lock.locked())
        waiter = asyncio.create_task(collect(self.pool, "three"))
        await asyncio.sleep(0)
        waiter.cancel()
        with self.assertRaises(asyncio.CancelledError):
            await waiter
        self.assertEqual(self.conn.waiting, 0)
        self.conn.release_rate_slot()
        await collect(self.pool, "four")

    async def test_cancel_during_gap_releases_gate(self):
        self.conn.request_delay = 1
        self.conn.last_fire = time.monotonic()
        waiter = asyncio.create_task(collect(self.pool, "one"))
        await asyncio.sleep(0.01)
        waiter.cancel()
        with self.assertRaises(asyncio.CancelledError):
            await waiter
        self.assertFalse(self.conn.rate_lock.locked())
        self.assertEqual(self.conn.waiting, 0)
        self.assertIsNone(self.conn.client)

    async def test_explicit_close_propagates_through_facade_to_backend(self):
        with patch.object(scheduler, "_pool", self.pool):
            response = scheduler.generate_response("one", "one", stream=True)
            self.assertEqual(await anext(response), "one")
            self.assertTrue(self.conn.rate_lock.locked())
            await response.aclose()
        self.assertEqual(FakeClient.events[-1][:2], ("end", "one"))
        self.assertFalse(self.conn.rate_lock.locked())
        self.assertFalse(self.pool._turn_locks["one"].locked())
        self.assertIsNone(self.conn.client._session_id)
        self.assertEqual(self.store.get_resume("one")[1], "parent:one")
        await collect(self.pool, "one", "next")

    async def test_cancellation_during_backend_releases_locks(self):
        await self.conn.ensure_client()
        self.conn.client.proceed.clear()
        task = asyncio.create_task(collect(self.pool, "one", stream=True))
        await self.conn.client.entered.wait()
        task.cancel()
        with self.assertRaises(asyncio.CancelledError):
            await task
        self.assertFalse(self.conn.rate_lock.locked())
        self.assertFalse(self.pool._turn_locks["one"].locked())
        self.assertIsNone(self.conn.client._session_id)

    async def test_turn_exit_closes_output_after_consumer_breaks(self):
        async with self.pool.turn("one") as turn:
            async for chunk in turn.generate("partial", stream=True):
                self.assertEqual(chunk, "partial")
                break
            self.assertTrue(self.conn.rate_lock.locked())
        self.assertFalse(self.conn.rate_lock.locked())
        self.assertEqual(FakeClient.events[-1][:2], ("end", "partial"))
        with self.assertRaises(RuntimeError):
            turn.generate("too late")
        await collect(self.pool, "one", "next")

    async def test_turn_exit_closes_output_when_consumer_is_cancelled(self):
        consuming = asyncio.Event()

        async def consumer():
            async with self.pool.turn("one") as turn:
                async for _ in turn.generate("partial", stream=True):
                    consuming.set()
                    await asyncio.Event().wait()

        task = asyncio.create_task(consumer())
        await consuming.wait()
        task.cancel()
        with self.assertRaises(asyncio.CancelledError):
            await task
        self.assertFalse(self.conn.rate_lock.locked())
        self.assertFalse(self.pool._turn_locks["one"].locked())
        self.assertEqual(FakeClient.events[-1][:2], ("end", "partial"))

    async def test_backoff_schedule_uses_existing_config(self):
        now = 100.0
        waits = []
        calls = []
        real_sleep = asyncio.sleep

        async def sleep(delay):
            nonlocal now
            waits.append(delay)
            now += delay
            await real_sleep(0)

        async def limited(*args, **kwargs):
            calls.append(now)
            raise FakeError("429 rate limit")
            yield  # async generator protocol

        with (
            patch.object(scheduler, "time", SimpleNamespace(monotonic=lambda: now)),
            patch.object(scheduler.asyncio, "sleep", sleep),
            patch.object(self.pool, "_generate_once", limited),
            patch.object(scheduler, "RATE_LIMIT_BACKOFF_S", 10),
            patch.object(scheduler, "RATE_LIMIT_MAX_RETRIES", 3),
        ):
            with self.assertRaises(scheduler.BackendRateLimited):
                await collect(self.pool, "one")
        self.assertEqual(waits, [10, 20, 30])
        self.assertEqual(len(calls), 4)
        self.assertEqual(self.conn.cooldown_until, now + 30)

    async def test_midstream_rate_limit_sets_cooldown_without_replay(self):
        async def limited(*args, **kwargs):
            yield "partial"
            raise FakeError("429 rate limit")

        with patch.object(self.pool, "_generate_once", limited):
            response = self.pool.generate_response("one", "one", stream=True)
            self.assertEqual(await anext(response), "partial")
            with self.assertRaises(FakeError):
                await anext(response)
        self.assertGreater(self.conn.cooldown_until, time.monotonic())
        self.assertFalse(self.conn.rate_lock.locked())

    async def test_rate_limit_cooldown_blocks_other_conversations(self):
        FakeClient.failures["limited"] = "429 too many requests"
        with (
            patch.object(scheduler, "RATE_LIMIT_BACKOFF_S", 0.5),
            patch.object(scheduler, "RATE_LIMIT_MAX_RETRIES", 1),
        ):
            await asyncio.gather(collect(self.pool, "one", "limited"), collect(self.pool, "two"))
        events = FakeClient.events
        self.assertGreaterEqual(events[2][2] - events[1][2], 0.49)
        self.assertEqual([e[1] for e in events if e[0] == "start"], ["limited", "two", "limited"])

    async def test_exhausted_limit_keeps_account_cooldown(self):
        FakeClient.failures["limited"] = "429 too many requests"
        with (
            patch.object(scheduler, "RATE_LIMIT_BACKOFF_S", 10),
            patch.object(scheduler, "RATE_LIMIT_MAX_RETRIES", 0),
        ):
            with self.assertRaises(scheduler.BackendRateLimited) as caught:
                await collect(self.pool, "one", "limited")
        self.assertEqual(caught.exception.retry_after, 10)
        self.assertGreater(self.conn.cooldown_until - time.monotonic(), 9)
        self.assertFalse(self.conn.rate_lock.locked())

    async def test_refresh_invalidates_all_old_client_conversations(self):
        await collect(self.pool, "one")
        old = self.conn.client
        FakeClient.failures["expired"] = "invalid token"
        await asyncio.gather(
            collect(self.pool, "two", "expired"), collect(self.pool, "one", "again")
        )
        self.assertTrue(old.closed)
        self.assertEqual(len(FakeClient.instances), 2)
        self.assertTrue(
            all(conv.client is self.conn.client for conv in self.conn.conversations.values())
        )
        self.assertEqual(self.store.get_binding("one"), "a")

    async def test_accounts_progress_independently_and_titles_are_ephemeral(self):
        pool = scheduler.ConnectionPool(
            [
                {"email": "a", "password": "fake"},
                {"email": "b", "password": "fake"},
            ],
            self.store,
            request_delay=0,
        )
        first = pool.route("one")
        await first.ensure_client()
        first.client.proceed.clear()
        task = asyncio.create_task(collect(pool, "one"))
        try:
            await first.client.entered.wait()
            self.assertEqual(await asyncio.wait_for(collect(pool, "two"), 0.5), ["two"])
        finally:
            first.client.proceed.set()
            await task
        await collect(pool, "title_example")
        self.assertIsNone(self.store.get_binding("title_example"))
        self.assertIsNone(self.store.get_resume("title_example"))


if __name__ == "__main__":
    unittest.main()
