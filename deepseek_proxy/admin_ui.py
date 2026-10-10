"""Self-contained admin management UI (no external assets, no build step)."""

from __future__ import annotations

ADMIN_HTML = """<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>DeepSeek Proxy — Admin</title>
<style>
  :root { color-scheme: dark light; }
  body { font-family: system-ui, sans-serif; margin: 0; padding: 2rem; line-height: 1.5; }
  main { max-width: 1000px; margin: 0 auto; }
  h1 { font-size: 1.35rem; margin-top: 0; }
  nav { display: flex; gap: .25rem; border-bottom: 1px solid #8884; margin-bottom: 1.25rem; }
  nav button { background: none; border: 0; border-bottom: 2px solid transparent; color: inherit; opacity: .6; padding: .5rem .8rem; font: inherit; font-size: .95rem; cursor: pointer; border-radius: 0; }
  nav button[aria-selected=true] { opacity: 1; border-bottom-color: #4f46e5; font-weight: 600; }
  [role=tabpanel][hidden] { display: none; }
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
  .toolbar { display: flex; gap: .75rem; align-items: center; flex-wrap: wrap; margin-top: 1rem; }
  .toolbar label { font-size: .85rem; opacity: .8; display: flex; gap: .35rem; align-items: center; }
  select, input[type=search] { padding: .4rem .5rem; border: 1px solid #8886; border-radius: 6px; background: transparent; color: inherit; font: inherit; font-size: .85rem; }
  input[type=search] { min-width: 14rem; }
  #logpane { margin-top: .75rem; border: 1px solid #8884; border-radius: 8px; height: 60vh; overflow: auto; padding: .5rem .6rem; font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: .8rem; line-height: 1.45; }
  .rec { white-space: pre-wrap; word-break: break-word; padding: .1rem 0; border-bottom: 1px solid #8881; }
  .rec .t { opacity: .55; }
  .rec .lv { font-weight: 700; }
  .rec .lg { opacity: .6; }
  .rec .rid { opacity: .75; }
  .rec.DEBUG .lv { color: #22d3ee; }
  .rec.INFO .lv { color: #22c55e; }
  .rec.WARNING .lv { color: #f59e0b; }
  .rec.ERROR .lv, .rec.CRITICAL .lv { color: #ef4444; }
  .rec .exc { color: #ef4444; }
</style>
</head>
<body>
<main>
  <h1>DeepSeek Proxy</h1>
  <nav>
    <button id="tab-keys" role="tab" aria-selected="true" aria-controls="panel-keys" type="button">API Keys</button>
    <button id="tab-logs" role="tab" aria-selected="false" aria-controls="panel-logs" type="button">Logs</button>
  </nav>

  <section id="panel-keys" role="tabpanel" aria-labelledby="tab-keys">
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
  </section>

  <section id="panel-logs" role="tabpanel" aria-labelledby="tab-logs" hidden>
    <p class="muted">In-process log buffer. It starts empty when the proxy restarts, and holds the most recent entries only.</p>
    <div class="toolbar">
      <label>Level
        <select id="level">
          <option value="">all</option>
          <option>DEBUG</option>
          <option selected>INFO</option>
          <option>WARNING</option>
          <option>ERROR</option>
          <option>CRITICAL</option>
        </select>
      </label>
      <label>Request ID <input type="search" id="rid" placeholder="req_…" autocomplete="off"></label>
      <label><input type="checkbox" id="follow" checked> Follow</label>
      <button class="ghost" id="clear" type="button">Clear view</button>
      <button class="ghost" id="refresh" type="button">Refresh</button>
    </div>
    <div class="err" id="logerr"></div>
    <div id="logpane" role="log" aria-live="polite"></div>
  </section>
</main>
<script>
  const $ = (id) => document.getElementById(id);
  const fmt = (t) => t ? new Date(t * 1000).toLocaleString() : "never";
  const esc = (s) => String(s).replace(/[&<>"']/g, (c) => ({"&":"&amp;","<":"&lt;",">":"&gt;",'"':"&quot;","'":"&#39;"}[c]));

  // ---- tabs ----
  const tabs = [["tab-keys", "panel-keys"], ["tab-logs", "panel-logs"]];
  function selectTab(id) {
    for (const [tabId, panelId] of tabs) {
      const on = tabId === id;
      $(tabId).setAttribute("aria-selected", on ? "true" : "false");
      $(panelId).hidden = !on;
    }
    if (id === "tab-logs") startLogs();
  }
  for (const [tabId] of tabs) $(tabId).addEventListener("click", () => selectTab(tabId));

  // ---- keys ----
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

  // ---- logs ----
  const MAX_ROWS = 50;
  // Rendered explicitly by the layout below; every other key on the record is
  // appended afterwards so no field is silently dropped.
  const RENDERED = ["seq", "time", "level", "logger", "msg", "request_id", "exc_info"];
  let lastSeq = 0, timer = null, polling = false;

  function line(r) {
    const t = new Date(r.time * 1000).toLocaleTimeString();
    let html = `<span class="t">${esc(t)}</span> <span class="lv">${esc(r.level)}</span> `;
    if (r.request_id) html += `<span class="rid">[${esc(r.request_id)}]</span> `;
    html += `<span class="lg">${esc(r.logger)}</span> ${esc(r.msg)}`;
    const rest = Object.keys(r).filter((k) => !RENDERED.includes(k)).sort();
    for (const k of rest) {
      const v = r[k];
      if (v === undefined || v === null) continue;
      html += ` ${esc(k)}=${esc(typeof v === "object" ? JSON.stringify(v) : v)}`;
    }
    if (r.exc_info) html += `<span class="exc">\\n${esc(r.exc_info)}</span>`;
    return `<div class="rec ${esc(r.level)}">${html}</div>`;
  }

  function append(records) {
    const pane = $("logpane");
    if (!records.length) return;
    if (lastSeq === 0) pane.innerHTML = "";
    pane.insertAdjacentHTML("beforeend", records.map(line).join(""));
    while (pane.childElementCount > MAX_ROWS) pane.removeChild(pane.firstElementChild);
    pane.scrollTop = pane.scrollHeight;
  }

  async function poll() {
    if (polling) return;
    polling = true;
    try {
      const params = new URLSearchParams({ after: String(lastSeq) });
      const lv = $("level").value;
      if (lv) params.set("level", lv);
      const rid = $("rid").value.trim();
      if (rid) params.set("request_id", rid);
      const res = await fetch("/admin/logs?" + params.toString());
      if (!res.ok) { $("logerr").textContent = "Log fetch failed (" + res.status + ")"; return; }
      $("logerr").textContent = "";
      const data = await res.json();
      if (!data.enabled) { $("logerr").textContent = "Log buffer disabled (set ADMIN_LOG_BUFFER > 0)."; return; }
      append(data.records);
      lastSeq = Math.max(lastSeq, data.last_seq || 0);
    } catch (e) {
      $("logerr").textContent = "Log fetch failed.";
    } finally {
      polling = false;
    }
  }

  function resetLogs() {
    lastSeq = 0;
    $("logpane").innerHTML = "";
    poll();
  }

  function startLogs() {
    if (timer === null) resetLogs();
    if (timer !== null) return;
    const tick = async () => {
      if ($("follow").checked && !$("panel-logs").hidden) await poll();
      timer = setTimeout(tick, 1500);
    };
    timer = setTimeout(tick, 1500);
  }

  $("level").addEventListener("change", resetLogs);
  $("rid").addEventListener("change", resetLogs);
  $("refresh").addEventListener("click", () => poll());
  $("clear").addEventListener("click", () => { $("logpane").innerHTML = ""; });
  $("follow").addEventListener("change", () => { if ($("follow").checked) poll(); });
  document.addEventListener("visibilitychange", () => { if (!document.hidden) poll(); });

  load();
</script>
</body>
</html>
"""
