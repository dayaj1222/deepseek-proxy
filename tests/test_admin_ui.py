from deepseek_proxy.admin_ui import ADMIN_HTML


def test_admin_html_self_contained():
    assert ADMIN_HTML.strip().startswith("<!DOCTYPE html>")
    assert "<script" in ADMIN_HTML
    assert "/admin/keys" in ADMIN_HTML
    assert "http://" not in ADMIN_HTML
    assert "https://" not in ADMIN_HTML
