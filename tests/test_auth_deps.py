import base64

from fastapi import Depends, FastAPI
from fastapi.testclient import TestClient

from deepseek_proxy.auth import hash_key, require_admin, require_api_key


class _FakeSettings:
    auth_enabled = True
    admin_user = "me"
    admin_pass = "pw"


class _FakeBackend:
    def __init__(self):
        self.rows = {}

    def find_api_key_by_hash(self, h):
        return self.rows.get(h)

    def touch_api_key_used(self, i, w):
        pass


class _FakeStore:
    def __init__(self):
        self._backend = _FakeBackend()


def _app():
    app = FastAPI()
    app.state.settings = _FakeSettings()
    app.state.store = _FakeStore()

    @app.get("/v1/x", dependencies=[Depends(require_api_key)])
    def x():
        return {"ok": True}

    @app.get("/admin/x", dependencies=[Depends(require_admin)])
    def ax():
        return {"ok": True}

    return app


def test_api_key_required_and_valid():
    c = TestClient(_app())
    assert c.get("/v1/x").status_code == 401
    c.app.state.store._backend.rows[hash_key("sk-ok")] = {"id": "1"}
    assert c.get("/v1/x", headers={"Authorization": "Bearer sk-ok"}).status_code == 200
    assert c.get("/v1/x", headers={"Authorization": "Bearer sk-bad"}).status_code == 401


def test_admin_basic():
    c = TestClient(_app())
    assert c.get("/admin/x").status_code == 401
    tok = base64.b64encode(b"me:pw").decode()
    assert c.get("/admin/x", headers={"Authorization": f"Basic {tok}"}).status_code == 200


def test_api_key_noop_when_disabled():
    c = TestClient(_app())
    c.app.state.settings.auth_enabled = False
    assert c.get("/v1/x").status_code == 200
