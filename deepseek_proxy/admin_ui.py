"""Self-contained admin management UI (no external assets, no build step)."""

from __future__ import annotations

ADMIN_HTML = """<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>DeepSeek Proxy — API Keys</title>
<style>
  :root { color-scheme: dark light; }
  body { font-family: system-ui, sans-serif; margin: 0; padding: 2rem; line-height: 1.5; }
  main { max-width: 780px; margin: 0 auto; }
  h1 { font-size: 1.35rem; margin-top: 0; }
  table { width: 100%; border-collapse: collapse; margin-top: 1rem; }
  th, td { text-align: left; padding: .5rem .6rem; border-bottom: 1px solid #8884; font-size: .92rem; }
  th { font-weight: 600; opacity: .75; }
  form { display: flex; gap: .5rem; margin-top: 1rem; }
  input[type=text] { flex: 1; padding: .5rem .6rem; border: 1px solid #8886; border-radius: 6px; background: transparent; color: inherit; }
  button { padding: .5rem .8rem; border: 1px solid #8886; border-radius: 6px; background: #4f46e5; color: #fff; cursor: pointer; font-size: .9rem; }
  button.ghost { background: transparent; color: inherit; }
  button:hover { filter: brightness(1.1); }
  .muted { opacity: .65; font-size: .85rem; }
  #reveal { display: none; margin-top: 1rem; padding: .8rem; border: 1px solid #f59e0b; border-radius: 8px; }
  #reveal code { display: block; margin-top: .5rem; padding: .6rem; background: #8882; border-radius: 6px; word-break: break-all; font-size: .85rem; }
  .err { color: #ef4444; margin-top: .5rem; min-height: 1.2em; font-size: .9rem; }
</style>
</head>
<body>
<main>
  <h1>API Keys</h1>
  <p class="muted">Keys grant access to the proxy (<code>/v1/*</code>). The raw key is shown once, at creation.</p>

  <form id="create">
    <input type="text" id="name" placeholder="Key name (e.g. laptop)" autocomplete="off" required>
    <button type="submit">Create key</button>
  </form>
  <div class="err" id="err"></div>

  <div id="reveal">
    <strong>Copy this key now — it will not be shown again.</strong>
    <code id="rawkey"></code>
    <button class="ghost" id="dismiss" type="button">Dismiss</button>
  </div>

  <table>
    <thead><tr><th>Name</th><th>Prefix</th><th>Created</th><th>Last used</th><th></th></tr></thead>
    <tbody id="rows"><tr><td colspan="5" class="muted">Loading…</td></tr></tbody>
  </table>
</main>
<script>
  const $ = (id) => document.getElementById(id);
  const fmt = (t) => t ? new Date(t * 1000).toLocaleString() : "never";
  const esc = (s) => String(s).replace(/[&<>"']/g, (c) => ({"&":"&amp;","<":"&lt;",">":"&gt;",'"':"&quot;","'":"&#39;"}[c]));

  async function load() {
    const res = await fetch("/admin/keys");
    if (!res.ok) { $("rows").innerHTML = '<tr><td colspan="5" class="muted">Failed to load.</td></tr>'; return; }
    const { keys } = await res.json();
    if (!keys.length) { $("rows").innerHTML = '<tr><td colspan="5" class="muted">No keys yet.</td></tr>'; return; }
    $("rows").innerHTML = keys.map((k) =>
      `<tr><td>${esc(k.name)}</td><td><code>${esc(k.key_prefix)}…</code></td>` +
      `<td class="muted">${fmt(k.created_at)}</td><td class="muted">${fmt(k.last_used_at)}</td>` +
      `<td><button class="ghost" data-id="${esc(k.id)}" type="button">Revoke</button></td></tr>`
    ).join("");
  }

  $("create").addEventListener("submit", async (e) => {
    e.preventDefault();
    $("err").textContent = "";
    const name = $("name").value.trim();
    if (!name) return;
    const res = await fetch("/admin/keys", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ name }),
    });
    if (!res.ok) { $("err").textContent = "Create failed (" + res.status + ")"; return; }
    const data = await res.json();
    $("rawkey").textContent = data.key;
    $("reveal").style.display = "block";
    $("name").value = "";
    load();
  });

  $("rows").addEventListener("click", async (e) => {
    const id = e.target && e.target.getAttribute && e.target.getAttribute("data-id");
    if (!id) return;
    if (!confirm("Revoke this key? Any client using it will stop working.")) return;
    const res = await fetch("/admin/keys/" + encodeURIComponent(id), { method: "DELETE" });
    if (!res.ok && res.status !== 404) { $("err").textContent = "Revoke failed (" + res.status + ")"; return; }
    load();
  });

  $("dismiss").addEventListener("click", () => { $("reveal").style.display = "none"; $("rawkey").textContent = ""; });

  load();
</script>
</body>
</html>
"""
