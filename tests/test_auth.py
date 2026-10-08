import base64
import hashlib

from deepseek_proxy.auth import (
    check_admin,
    generate_key,
    hash_key,
    parse_basic,
    parse_bearer,
)


def test_generate_key_format():
    k = generate_key()
    assert k.startswith("sk-")
    assert len(k) == 3 + 43
    assert generate_key() != generate_key()


def test_hash_key_deterministic():
    assert hash_key("sk-abc") == hashlib.sha256(b"sk-abc").hexdigest()


def test_check_admin():
    assert check_admin("me", "pw", "me", "pw") is True
    assert check_admin("me", "bad", "me", "pw") is False
    assert check_admin("", "", "me", "pw") is False


def test_parse_basic():
    tok = base64.b64encode(b"me:pw").decode()
    assert parse_basic(f"Basic {tok}") == ("me", "pw")
    assert parse_basic(None) is None
    assert parse_basic("Bearer x") is None
    assert parse_basic("Basic not-base64!!!") is None


def test_parse_bearer():
    assert parse_bearer("Bearer sk-abc") == "sk-abc"
    assert parse_bearer(None) is None
    assert parse_bearer("Bearer") is None
    assert parse_bearer("Basic zzz") is None
    assert parse_bearer("Bearer ") is None
