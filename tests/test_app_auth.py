import base64
import dataclasses

from fastapi.testclient import TestClient

from deepseek_proxy.app import create_app
from deepseek_proxy.settings import settings as base_settings


def _settings(**over):
    # Auth tests exercise HTTP auth only, but app startup builds a
    # ConnectionPool that requires at least one account. The test harness
    # blanks DEEPSEEK_ENV_FILE, so base_settings normally has no accounts;
    # inject a dummy one so these tests stay self-contained instead of
    # depending on ambient credentials.
    if not base_settings.accounts:
        over.setdefault("accounts", [{"email": "test@example.com", "password": "pw"}])
    return dataclasses.replace(base_settings, **over)


def _client(auth_enabled, admin_user="me", admin_pass="pw"):
    app = create_app(
        settings=_settings(auth_enabled=auth_enabled, admin_user=admin_user, admin_pass=admin_pass)
    )
    return TestClient(app)


def test_local_default_is_open_and_no_admin(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    c = _client(auth_enabled=False)
    with c:
        assert c.get("/v1/models").status_code == 200
        assert c.get("/admin").status_code == 404


def test_enabled_requires_key_and_admin(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    c = _client(auth_enabled=True)
    with c:
        assert c.get("/v1/models").status_code == 401
        tok = base64.b64encode(b"me:pw").decode()
        admin = {"Authorization": f"Basic {tok}"}
        assert c.get("/admin", headers=admin).status_code == 200
        created = c.post("/admin/keys", json={"name": "t"}, headers=admin)
        assert created.status_code == 200
        key = created.json()["key"]
        assert c.get("/v1/models", headers={"Authorization": f"Bearer {key}"}).status_code == 200
        kid = created.json()["id"]
        assert c.delete(f"/admin/keys/{kid}", headers=admin).status_code == 200
        assert c.get("/v1/models", headers={"Authorization": f"Bearer {key}"}).status_code == 401
