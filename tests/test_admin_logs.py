import base64
import dataclasses
import logging

from fastapi.testclient import TestClient

from deepseek_proxy.app import create_app
from deepseek_proxy.observability import LogRingHandler, _level_value, get_log_buffer
from deepseek_proxy.settings import settings as base_settings


def _record(msg="hello", level=logging.INFO, logger="deepseek_proxy.test", **extra):
    rec = logging.LogRecord(logger, level, __file__, 1, msg, (), None)
    for k, v in extra.items():
        setattr(rec, k, v)
    return rec


def test_ring_handler_keeps_newest_and_assigns_seq():
    h = LogRingHandler(3)
    for i in range(5):
        h.emit(_record(f"m{i}"))
    out = h.snapshot()
    assert [r["msg"] for r in out] == ["m2", "m3", "m4"]
    assert [r["seq"] for r in out] == [3, 4, 5]
    assert h.last_seq == 5


def test_ring_handler_after_and_level_filters():
    h = LogRingHandler(10)
    h.emit(_record("debug", logging.DEBUG))
    h.emit(_record("info", logging.INFO))
    h.emit(_record("warn", logging.WARNING))
    h.emit(_record("err", logging.ERROR))

    assert [r["msg"] for r in h.snapshot(after=2)] == ["warn", "err"]
    assert [r["msg"] for r in h.snapshot(level="WARNING")] == ["warn", "err"]
    assert [r["msg"] for r in h.snapshot(level="DEBUG")] == ["debug", "info", "warn", "err"]
    assert [r["msg"] for r in h.snapshot(limit=2)] == ["warn", "err"]


def test_ring_handler_request_id_filter():
    h = LogRingHandler(10)
    h.emit(_record("a", request_id="req_1"))
    h.emit(_record("b", request_id="req_2"))
    assert [r["msg"] for r in h.snapshot(request_id="req_1")] == ["a"]


def test_ring_handler_carries_structured_fields():
    h = LogRingHandler(10)
    h.emit(_record("x", model="deepseek-chat", duration_ms=12.5))
    rec = h.snapshot()[0]
    assert rec["model"] == "deepseek-chat"
    assert rec["duration_ms"] == 12.5
    assert rec["level"] == "INFO"


def test_level_value_handles_junk():
    assert _level_value(None) is None
    assert _level_value("") is None
    assert _level_value("WARNING") == logging.WARNING
    assert _level_value("not-a-level") is None


def test_zero_capacity_keeps_nothing():
    h = LogRingHandler(0)
    h.emit(_record("dropped"))
    assert h.snapshot() == []


def _settings(**over):
    if not base_settings.accounts:
        over.setdefault("accounts", [{"email": "test@example.com", "password": "pw"}])
    return dataclasses.replace(base_settings, **over)


def _admin():
    tok = base64.b64encode(b"me:pw").decode()
    return {"Authorization": f"Basic {tok}"}


def test_logs_endpoint_requires_admin():
    app = create_app(settings=_settings(auth_enabled=True, admin_user="me", admin_pass="pw"))
    with TestClient(app) as c:
        assert c.get("/admin/logs").status_code == 401
        assert c.get("/admin/logs", headers=_admin()).status_code == 200


def test_logs_endpoint_returns_buffered_records():
    app = create_app(settings=_settings(auth_enabled=True, admin_user="me", admin_pass="pw"))
    with TestClient(app) as c:
        buf = get_log_buffer()
        assert buf is not None
        logging.getLogger("deepseek_proxy.test").warning("buffer me")
        body = c.get("/admin/logs", headers=_admin()).json()
        assert body["enabled"] is True
        assert any(r["msg"] == "buffer me" for r in body["records"])
        assert body["last_seq"] >= 1


def test_logs_endpoint_rejects_bad_level():
    app = create_app(settings=_settings(auth_enabled=True, admin_user="me", admin_pass="pw"))
    with TestClient(app) as c:
        res = c.get("/admin/logs?level=NOPE", headers=_admin())
        assert res.status_code == 400


def test_logs_endpoint_absent_when_auth_disabled(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    app = create_app(settings=_settings(auth_enabled=False))
    with TestClient(app) as c:
        assert c.get("/admin/logs").status_code == 404
