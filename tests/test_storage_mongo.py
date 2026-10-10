"""Behavioral tests for the MongoDB backend.

Skipped unless MONGO_TEST_URI points at a reachable server, so the default
`pytest` run (and CI without a Mongo service) stays green and SQLite-only.

Run locally with, e.g.:

    MONGO_TEST_URI=mongodb://localhost:27017 pytest tests/test_storage_mongo.py

The suite reuses a scratch database per test and drops it on teardown.
"""

from __future__ import annotations

import os
import unittest

from db import StateStore
from deepseek_proxy.storage import MongoBackend, build_backend

MONGO_TEST_URI = os.environ.get("MONGO_TEST_URI", "")
MONGO_TEST_DB = os.environ.get("MONGO_TEST_DB", "deepseek_proxy_test")


def _mongo_available() -> bool:
    if not MONGO_TEST_URI:
        return False
    try:
        import pymongo  # noqa: F401
    except ImportError:
        return False
    try:
        from pymongo import MongoClient

        client = MongoClient(MONGO_TEST_URI, serverSelectionTimeoutMS=1500)
        client.admin.command("ping")
        client.close()
        return True
    except Exception:
        return False


@unittest.skipUnless(_mongo_available(), "MONGO_TEST_URI not set or Mongo unreachable")
class MongoStorageTests(unittest.TestCase):
    def setUp(self):
        self.store = StateStore(
            "unused.db",
            snapshot_interval=0,
            backend=MongoBackend(MONGO_TEST_URI, MONGO_TEST_DB),
        )
        self.store.open()
        self.addCleanup(self._drop_and_close)

    def _drop_and_close(self):
        backend = self.store._backend
        client = getattr(backend, "_client", None)
        if client is not None:
            client.drop_database(MONGO_TEST_DB)
        self.store.close()

    def test_bind_and_get_binding_round_trips(self):
        self.store.bind("thread", "account")
        self.store.flush()
        # Reload from Mongo into a fresh store to prove persistence.
        fresh = StateStore(
            "unused.db", snapshot_interval=0, backend=MongoBackend(MONGO_TEST_URI, MONGO_TEST_DB)
        )
        fresh.open()
        try:
            self.assertEqual(fresh.get_binding("thread"), "account")
        finally:
            fresh.close()

    def test_resume_and_counters_persist(self):
        self.store.set_resume("thread", "account", "session", "parent")
        self.store.add_thread_tokens("thread", 42)
        self.store.bump_thread_exchanges("thread", 2)
        self.store.flush()

        fresh = StateStore(
            "unused.db", snapshot_interval=0, backend=MongoBackend(MONGO_TEST_URI, MONGO_TEST_DB)
        )
        fresh.open()
        try:
            self.assertEqual(fresh.get_resume("thread"), ("session", "parent"))
            self.assertEqual(fresh.get_thread_tokens("thread"), 42)
            self.assertEqual(fresh.get_thread_exchanges("thread"), 2)
        finally:
            fresh.close()

    def test_mutations_do_not_write_until_flush(self):
        self.store.bind("thread", "account")
        # No flush yet: a second store must not see the binding.
        fresh = StateStore(
            "unused.db", snapshot_interval=0, backend=MongoBackend(MONGO_TEST_URI, MONGO_TEST_DB)
        )
        fresh.open()
        try:
            self.assertIsNone(fresh.get_binding("thread"))
        finally:
            fresh.close()

    def test_build_backend_selects_mongo(self):
        backend = build_backend(
            "mongo", db_path="x.db", mongo_uri=MONGO_TEST_URI, mongo_db=MONGO_TEST_DB
        )
        self.assertIsInstance(backend, MongoBackend)

    def test_build_backend_rejects_unknown(self):
        with self.assertRaises(ValueError):
            build_backend("postgres", db_path="x.db")


if __name__ == "__main__":
    unittest.main()
