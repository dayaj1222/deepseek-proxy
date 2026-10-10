from deepseek_proxy.admin_ui import ADMIN_HTML


def test_admin_html_self_contained():
    assert ADMIN_HTML.strip().startswith("<!DOCTYPE html>")
    assert "<script" in ADMIN_HTML
    assert "/admin/keys" in ADMIN_HTML
    assert "http://" not in ADMIN_HTML
    assert "https://" not in ADMIN_HTML


def test_admin_html_has_logs_tab():
    assert "/admin/logs" in ADMIN_HTML
    assert 'id="panel-logs"' in ADMIN_HTML
    # Tabs must start with the keys panel active so existing behaviour is intact.
    assert 'id="tab-keys" role="tab" aria-selected="true"' in ADMIN_HTML
