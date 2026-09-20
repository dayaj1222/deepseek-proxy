import asyncio
from pathlib import Path
import sqlite3
import tempfile
import threading
import unittest
from unittest.mock import patch

from db import StateStore


class StorageTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.path = Path(self.temp.name) / "state.db"
        self.store = StateStore(str(self.path), snapshot_interval=0)
        self.store.open()
        self.addCleanup(self.store.close)

    def read(self, query):
        with sqlite3.connect(self.path) as conn:
            return conn.execute(query).fetchall()

    def test_mutations_never_write_synchronously(self):
        with patch.object(self.store, "snapshot", side_effect=AssertionError("synchronous write")):
            self.store.bind("thread", "account")
            self.store.touch("thread")
            self.store.set_resume("thread", "account", "session", "parent")
            self.store.add_thread_tokens("thread", 42)
            self.store.bump_thread_exchanges("thread", 2)
        self.assertEqual(self.read("SELECT * FROM threads"), [])
        self.store.flush()
        self.assertEqual(self.read("SELECT total_tokens, exchanges FROM resume"), [(42, 2)])

    def test_touch_persists_and_flush_is_incremental(self):
        for tid in ("one", "two"):
            self.store.bind(tid, "account")
            self.store.set_resume(tid, "account", "session", None)
        self.store.flush()
        changed = self.store._conn.total_changes
        with patch("db.time.time", return_value=1234.5):
            self.store.touch("one")
        self.store.flush()
        self.assertEqual(self.store._conn.total_changes - changed, 1)
        self.assertEqual(
            self.read("SELECT last_active FROM threads WHERE thread_id = 'one'"), [(1234.5,)]
        )
        self.store.flush()
        self.assertEqual(self.store._conn.total_changes - changed, 1)

    def test_reload_retains_backend_email_and_counters(self):
        self.store.set_resume("thread", "account", "session", "parent")
        self.store.close()
        self.store.open()
        self.store.add_thread_tokens("thread", 10)
        self.store.bump_thread_exchanges("thread", 1)
        self.store.flush()
        self.assertEqual(
            self.read("SELECT backend_email, total_tokens, exchanges FROM resume"),
            [("account", 10, 1)],
        )
        self.assertEqual(self.store.get_resume("thread"), ("session", "parent"))

    def test_counter_only_resume_uses_binding_email(self):
        self.store.bind("thread", "account")
        self.store.add_thread_tokens("thread", 10)
        self.store.flush()
        self.assertEqual(self.read("SELECT backend_email FROM resume"), [("account",)])

    def test_failed_flush_retries_dirty_rows(self):
        self.store.bind("thread", "account")
        self.store._conn.execute(
            "CREATE TRIGGER fail_insert BEFORE INSERT ON threads BEGIN SELECT RAISE(FAIL, 'test'); END"
        )
        with self.assertRaises(sqlite3.IntegrityError):
            self.store.flush()
        self.assertTrue(self.store._dirty)
        self.store._conn.execute("DROP TRIGGER fail_insert")
        self.store.flush()
        self.assertEqual(self.read("SELECT thread_id FROM threads"), [("thread",)])

    def test_to_thread_flush_preserves_concurrent_mutation(self):
        self.store.bind("thread", "account")
        self.store.set_resume("thread", "account", "session", None)
        entered, release = threading.Event(), threading.Event()

        def trace(sql):
            if sql == "BEGIN":
                entered.set()
                release.wait(timeout=5)

        self.store._conn.set_trace_callback(trace)

        async def run():
            flushing = asyncio.create_task(asyncio.to_thread(self.store.flush))
            try:
                self.assertTrue(await asyncio.to_thread(entered.wait, 2))
                self.store.add_thread_tokens("thread", 17)
            finally:
                release.set()
                await flushing
            self.store._conn.set_trace_callback(None)
            await asyncio.to_thread(self.store.flush)

        asyncio.run(run())
        self.assertEqual(self.read("SELECT total_tokens FROM resume"), [(17,)])
        self.assertFalse(self.store._dirty)

    def test_close_flushes_and_can_be_repeated(self):
        self.store.bind("thread", "account")
        self.store.close()
        self.store.close()
        self.assertEqual(self.read("SELECT thread_id FROM threads"), [("thread",)])


if __name__ == "__main__":
    unittest.main()
