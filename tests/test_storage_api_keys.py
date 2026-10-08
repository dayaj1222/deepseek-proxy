import tempfile
from pathlib import Path

from deepseek_proxy.storage import SqliteBackend


def _backend():
    d = tempfile.mkdtemp()
    b = SqliteBackend(str(Path(d) / "k.db"))
    b.open()
    return b


def test_insert_find_delete_touch():
    b = _backend()
    rec = {
        "id": "id1",
        "key_hash": "h1",
        "key_prefix": "sk-a",
        "name": "first",
        "created_at": 1.0,
        "last_used_at": None,
    }
    b.insert_api_key(rec)
    assert b.find_api_key_by_hash("h1")["name"] == "first"
    b.touch_api_key_used("id1", 2.0)
    assert b.find_api_key_by_hash("h1")["last_used_at"] == 2.0
    assert b.delete_api_key("id1") is True
    assert b.find_api_key_by_hash("h1") is None
    assert b.delete_api_key("id1") is False
    b.close()


def test_list_excludes_hash_and_is_newest_first():
    b = _backend()
    b.insert_api_key(
        {"id": "a", "key_hash": "ha", "key_prefix": "sk-a", "name": "a", "created_at": 1.0, "last_used_at": None}
    )
    b.insert_api_key(
        {"id": "b", "key_hash": "hb", "key_prefix": "sk-b", "name": "b", "created_at": 2.0, "last_used_at": None}
    )
    rows = b.list_api_keys()
    assert [r["id"] for r in rows] == ["b", "a"]
    assert all("key_hash" not in r for r in rows)
    b.close()
