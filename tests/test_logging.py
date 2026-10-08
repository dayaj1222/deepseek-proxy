"""Log-shape tests: production stays sparse, debug is explicit.

Run with: .venv/bin/python -m pytest tests/test_logging.py
"""

import importlib
import json
import logging
import unittest
from unittest.mock import patch

from deepseek_proxy.observability import STRUCTURED_FIELDS


def render(level, fmt, debug):
    """Configure logging for `fmt` and return the formatted line for a record."""
    with patch("deepseek_proxy.observability.settings") as fake:
        fake.log_level = level
        fake.log_format = fmt
        fake.debug = debug
        import deepseek_proxy.observability as obs

        importlib.reload(obs)
        records = []

        class Capture(logging.Handler):
            def emit(self, record):
                records.append(obs.JsonFormatter().format(record))

        root = logging.getLogger()
        old_handlers, old_level = root.handlers[:], root.level
        root.handlers.clear()
        # get_logger triggers _configure() (which clears handlers), so let it
        # run first, then attach the capture handler.
        obs.get_logger("t")
        root.addHandler(Capture())
        root.setLevel(logging.DEBUG)
        try:
            log = logging.getLogger("t")
            log.info(
                "Turn complete",
                extra={
                    "thread_id": "thread_abc",
                    "prompt_tokens": 25817,
                    "new_messages": 3,
                },
            )
            log.debug(
                "Prepared request",
                extra={"thread_id": "thread_abc", "reanchor": True, "prompt_tokens": 900},
            )
            log.warning("Tool repair", extra={"thread_id": "thread_abc", "unresolved": 2})
        finally:
            root.handlers[:] = old_handlers
            root.setLevel(old_level)
    return records


class StructuredFieldTests(unittest.TestCase):
    def test_json_promotes_fields_out_of_the_message(self):
        lines = render("INFO", "json", False)
        payload = json.loads(lines[0])
        self.assertEqual(payload["msg"], "Turn complete")
        self.assertEqual(payload["thread_id"], "thread_abc")
        self.assertEqual(payload["prompt_tokens"], 25817)
        self.assertNotIn("thread=", payload["msg"])

    def test_prod_info_level_hides_debug_detail(self):
        """At LOG_LEVEL=INFO the debug chatter is not emitted at all."""
        with patch("deepseek_proxy.observability.settings") as fake:
            fake.log_level = "INFO"
            fake.log_format = "json"
            fake.debug = False
            import deepseek_proxy.observability as obs

            importlib.reload(obs)
            emitted = []
            root = logging.getLogger()
            old_handlers, old_level = root.handlers[:], root.level
            root.handlers.clear()
            obs._configure()
            root.handlers[0].emit = lambda r: emitted.append(r.getMessage())
            try:
                obs.get_logger("t").debug("Prepared request", extra={"thread_id": "x"})
            finally:
                root.handlers[:] = old_handlers
                root.setLevel(old_level)
        self.assertEqual(emitted, [])

    def test_debug_level_emits_explicit_detail(self):
        lines = render("DEBUG", "json", True)
        self.assertTrue(any("Prepared request" in line for line in lines))
        detail = json.loads(lines[1])
        self.assertEqual(detail["thread_id"], "thread_abc")
        self.assertIs(detail["reanchor"], True)

    def test_allowlist_rejects_arbitrary_extras(self):
        """A caller cannot leak an arbitrary attribute into the log."""
        lines = render("INFO", "json", False)
        log = logging.getLogger("t")
        record = logging.LogRecord("t", logging.INFO, __file__, 1, "m", (), None)
        record.secret_payload = "SENSITIVE"
        with patch("deepseek_proxy.observability.settings") as fake:
            fake.debug = False
            fake.log_format = "json"
            import deepseek_proxy.observability as obs

            importlib.reload(obs)
            out = obs.JsonFormatter().format(record)
        self.assertNotIn("SENSITIVE", out)
        self.assertNotIn("secret_payload", out)
        self.assertNotIn("secret_payload", STRUCTURED_FIELDS)
        del log, lines


if __name__ == "__main__":
    unittest.main()
