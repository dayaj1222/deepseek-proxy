import importlib.util
import json
import logging
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[1]
PACKAGE = ROOT / "deepseek_proxy"


def load_config(path, env=None):
    spec = importlib.util.spec_from_file_location(
        "deepseek_proxy.settings", PACKAGE / "settings.py"
    )
    module = importlib.util.module_from_spec(spec)
    # DEEPSEEK_ENV_FILE="" disables .env loading so a developer's real .env
    # cannot leak into the deliberately clean environment these tests build.
    with patch.dict(
        os.environ,
        {"DEEPSEEK_CONFIG": str(path), "DEEPSEEK_ENV_FILE": "", **(env or {})},
        clear=True,
    ):
        with patch.dict(sys.modules, {spec.name: module}):
            spec.loader.exec_module(module)
    return module


class ConfigTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.path = Path(self.temp.name) / "config.toml"

    def test_defaults(self):
        config = load_config(self.path)
        self.assertEqual(config.REQUEST_DELAY, 2)
        self.assertEqual(config.QUEUE_LIMIT, 64)
        self.assertEqual(config.QUEUE_TIMEOUT, 120)
        self.assertEqual(config.REQUEST_TIMEOUT, 300)
        self.assertEqual(config.HEARTBEAT_INTERVAL, 10)
        self.assertEqual(config.IMAGE_MAX_BYTES, 10 * 1024 * 1024)
        self.assertEqual(config.RATE_LIMIT_BACKOFF_S, 10)
        self.assertEqual(config.RATE_LIMIT_MAX_RETRIES, 3)
        self.assertEqual([m["id"] for m in config.MODELS], ["DEFAULT", "EXPERT", "VISION"])
        self.assertEqual(config.CONFIG_PATH, self.path)
        self.assertEqual(config._TOML, {})
        self.assertTrue(callable(config.render_prompt))
        self.assertIn("TOOL_CALL_TEMPLATE", config.__all__)

    def test_precedence_and_shared_toml(self):
        self.path.write_text('QUEUE_LIMIT = 8\nTOOL_FORMAT = "dsml"\n')
        config = load_config(self.path, {"QUEUE_LIMIT": "12"})
        self.assertEqual(config.QUEUE_LIMIT, 12)
        self.assertEqual(config._TOML["QUEUE_LIMIT"], 8)
        self.assertEqual(config.settings.tool_format, "dsml")

    def test_malformed_toml_fails(self):
        self.path.write_text('SECRET = "unterminated')
        with self.assertRaisesRegex(ValueError, "Invalid TOML configuration"):
            load_config(self.path)

    def test_invalid_numeric_environment(self):
        for key, value in (
            ("QUEUE_LIMIT", "0"),
            ("QUEUE_LIMIT", "1.5"),
            ("PROXY_PORT", "65536"),
            ("REQUEST_DELAY", "-1"),
            ("REQUEST_TIMEOUT", "nan"),
            ("QUEUE_TIMEOUT", "inf"),
            ("HEARTBEAT_INTERVAL", "0"),
            ("IMAGE_MAX_BYTES", "-1"),
            ("RATE_LIMIT_MAX_RETRIES", "-1"),
            ("RATE_LIMIT_BACKOFF_S", "0"),
            ("TOOL_BUFFER_LIMIT", "0"),
            ("SYSTEM_PROMPT_INTERVAL", "-1"),
            ("TOOL_REPAIR_MAX_RETRIES", "-1"),
            ("IDLE_TIMEOUT", "-1"),
        ):
            with self.subTest(key=key, value=value):
                with self.assertRaisesRegex(ValueError, key):
                    load_config(self.path, {key: value})

    def test_toml_integer_rejects_float_and_bool(self):
        for value in ("1.5", "true"):
            self.path.write_text(f"QUEUE_LIMIT = {value}\n")
            with self.assertRaisesRegex(ValueError, "QUEUE_LIMIT"):
                load_config(self.path)

    def test_direct_settings_are_validated(self):
        config = load_config(self.path)
        with patch.dict(os.environ, {}, clear=True):
            with self.assertRaisesRegex(ValueError, "REQUEST_TIMEOUT"):
                config.Settings(request_timeout=float("nan"))
            self.assertEqual(config.Settings(request_delay=0).request_delay, 0)

    def test_token_only_and_indexed_accounts(self):
        config = load_config(self.path, {"DEEPSEEK_TOKEN": "secret"})
        self.assertEqual(
            config.ACCOUNTS, [{"email": "token-account", "password": "", "token": "secret"}]
        )
        self.assertNotIn("secret", repr(config.settings))
        accounts = config._load_accounts({"DEEPSEEK_TOKEN_0": "one", "DEEPSEEK_TOKEN_1": "two"}, {})
        self.assertEqual([a["email"] for a in accounts], ["token-account-0", "token-account-1"])
        self.assertEqual(
            config._load_accounts({"DEEPSEEK_EMAIL": "email", "DEEPSEEK_PASSWORD": "pw"}, {}),
            [{"email": "email", "password": "pw"}],
        )

    def test_import_and_empty_estimate_are_lazy(self):
        result = subprocess.run(
            [
                sys.executable,
                "-c",
                "import sys, config; assert 'tiktoken' not in sys.modules; "
                "assert config is sys.modules['deepseek_proxy.settings']; "
                "assert 'deepseek_proxy.tools.format' not in sys.modules; "
                "assert 'tool_format' not in sys.modules; assert config.estimate_tokens('') == 0; "
                "assert 'tiktoken' not in sys.modules",
            ],
            cwd=ROOT,
            env={"DEEPSEEK_CONFIG": str(self.path)},
            capture_output=True,
            text=True,
        )
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_estimator_initializes_on_first_nonempty_text(self):
        from types import SimpleNamespace
        from unittest.mock import Mock

        config = load_config(self.path)
        encoder = SimpleNamespace(encode=Mock(return_value=[1, 2]))
        get_encoding = Mock(return_value=encoder)
        with patch.dict(sys.modules, {"tiktoken": SimpleNamespace(get_encoding=get_encoding)}):
            self.assertEqual(config.estimate_tokens("text"), 2)
            self.assertEqual(config.estimate_tokens("again"), 2)
        get_encoding.assert_called_once_with("cl100k_base")

    def test_logger_context_and_safe_timing_fields(self):
        config = load_config(self.path)
        spec = importlib.util.spec_from_file_location(
            "deepseek_proxy.observability", PACKAGE / "observability.py"
        )
        logger = importlib.util.module_from_spec(spec)
        with patch.dict(sys.modules, {"deepseek_proxy.settings": config, spec.name: logger}):
            spec.loader.exec_module(logger)
        record = logging.LogRecord("test", logging.INFO, "", 0, "done %s", ("ok",), None)
        record.duration_ms = 12.5
        record.password = "secret"
        record.tool = "private-tool"
        outer = logger.set_request_id("outer")
        try:
            with logger.request_context("inner"):
                payload = json.loads(logger.JsonFormatter().format(record))
                self.assertEqual(payload["request_id"], "inner")
                self.assertEqual(payload["duration_ms"], 12.5)
                self.assertNotIn("args", payload)
                self.assertNotIn("secret", json.dumps(payload))
                self.assertNotIn("private-tool", json.dumps(payload))
                self.assertIn("duration_ms=12.5", logger.PrettyFormatter().format(record))
            self.assertEqual(logger._request_id.get(), "outer")
        finally:
            logger.clear_request_id(outer)
        self.assertIsNone(logger._request_id.get())


if __name__ == "__main__":
    unittest.main()
