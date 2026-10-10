import importlib.util
import os
import sys
from pathlib import Path
from unittest.mock import patch

import pytest

ROOT = Path(__file__).resolve().parents[1]
PACKAGE = ROOT / "deepseek_proxy"


def load_config(path, env=None):
    spec = importlib.util.spec_from_file_location(
        "deepseek_proxy.settings", PACKAGE / "settings.py"
    )
    module = importlib.util.module_from_spec(spec)
    with patch.dict(
        os.environ,
        {"DEEPSEEK_CONFIG": str(path), "DEEPSEEK_ENV_FILE": "", **(env or {})},
        clear=True,
    ):
        with patch.dict(sys.modules, {spec.name: module}):
            spec.loader.exec_module(module)
    return module


def test_auth_defaults_off(tmp_path):
    cfg = load_config(tmp_path / "config.toml")
    assert cfg.AUTH_ENABLED is False
    assert cfg.ADMIN_USER == ""
    assert cfg.ADMIN_PASS == ""


def test_auth_enabled_requires_admin_creds(tmp_path):
    with pytest.raises(ValueError, match="ADMIN_USER and ADMIN_PASS"):
        load_config(tmp_path / "config.toml", {"AUTH_ENABLED": "true"})


def test_auth_enabled_with_creds_ok(tmp_path):
    cfg = load_config(
        tmp_path / "config.toml",
        {"AUTH_ENABLED": "true", "ADMIN_USER": "me", "ADMIN_PASS": "pw"},
    )
    assert cfg.AUTH_ENABLED is True
    assert cfg.ADMIN_USER == "me"
