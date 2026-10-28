# CHANGELOG

Every landed change with the reasoning behind it. Newest first. History before
2026-10-28 was removed on purpose; see `git log -- docs/CHANGELOG.md`.
Decisions are `ADR-nnnn` in [`adr/`](adr/).

---

## 2026-10-28 - Native dialect is the default; admin routes need no key

**Tool calls are no longer taught to the model by default.** The `native`
dialect names no tag, marker or example; the model writes calls in its own
spelling and the proxy reads them back. Teaching a format is the exposure we are
avoiding (ADR-0014). `json_invoke` and `simple_tool_tag` remain as opt-in
dialects (`DIALECT` env or `[proxy] dialect`). `config.toml` no longer pins the
dialect, so a binary that predates `native` is not broken by an unknown name.

**`GET /admin/throttled` and `POST /admin/rebind` no longer check an API key**,
so `deepseek-rs rebind` works without `--key` (ADR-0015). The port binds
`0.0.0.0`; keep it off untrusted networks.

Also: the pending dead-code cleanup in pool, session, store and threads landed
with this change, and two test helpers stopped setting the removed `Msg.name`.

**Native repair no longer shows a format either.** Repair tiers used to restate
a concrete DSML example; in native mode they now say only to emit the call in the
model's own format, and the tier wording drops the "which tag opens the block"
prompting. Test: `native_repair_prompts_never_spell_out_markup`.

**The native per-turn reminder now says "DSML tool"**, because the model was
emitting the old simple-tag spelling on threads that predate native. The word is
an anchor; no tag is spelled (test `teaches_no_wire_format_at_all`).

Known gap left open: `deepseek-rs rebind --yes` without `--all` or a thread
silently stays a dry run.

*Files:* `config.toml`, `crates/proxy/src/{main,dialect,pool,session,store,threads}.rs`,
`crates/proxy/tests/http.rs`
