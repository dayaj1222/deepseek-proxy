# CHANGELOG

Every landed change with the reasoning behind it. Newest first. History before
2026-10-28 was removed on purpose; see `git log -- docs/CHANGELOG.md`.
Decisions are `ADR-nnnn` in [`adr/`](adr/).

---

## 2026-11-01 - `main.rs` split, second cut: request shape and admin routes

**The first cut left `main.rs` at 3585 lines**, still holding the request model,
its validation, the whole admin surface and the build stamp. Those had no
reason to be next to the streaming handler.

Two more modules, bodies moved rather than rewritten:

- `request.rs` - `ChatRequest`, `Msg`, `resolve_tools`, `parse_target`,
  `rebind_header`, `validate_request`, `has_external_ref`, `is_supported_model`.
  Deserialization and validation sit in one file on purpose: the serde
  attributes say what is *accepted on the wire*, `validate_request` says what
  is *semantically allowed*, and a change to one usually needs the other.
- `admin.rs` - `admin_throttled`, `admin_rebind`, `RebindRequest`,
  `clone_change`, `build_id`, `civil_from_days` and the build-stamp test. The
  admin surface is what `deepseek-rs rebind` and `status` talk to, so it now
  reads as one thing instead of being interleaved with `/v1/models`.

`backend_error` moved to `error.rs`, where the other failure-to-response
mapping already lives. `main.rs` re-exports the request types and functions, so
`turn.rs`, `prompt.rs` and `images.rs` keep going through `crate::` unchanged.

Tests: none changed; the build-stamp test moved with `civil_from_days`.
`cargo test --workspace` is 263 green and clippy still reports the same 28
warnings - two of them now point at `request.rs` instead of `main.rs`.

Accepted cost: `RebindRequest` is now `pub(crate)` only so `build_router` can
name it; the admin routes remain ordinary API routes (they still go through
`authorize`), which is the deliberate choice in ADR-0015 and not re-opened here.

Next cut, still out of scope: `stream_response` and the repair loop.

---

## 2026-11-01 - `main.rs` split into `state` and `error`

**`main.rs` had grown to ~3900 lines.** It held the shared state, both error
types, the router, every handler and the whole test module, so finding the one
place a rule lived meant scanning a file that had stopped being readable.

Two modules were extracted, unchanged in behaviour:

- `state.rs` - `Settings`, `AppState`, `SettingsHandle`, `Stats` and
  `restart_only_keys`. Everything shared by every request now lives in one
  place, and the reload story (generation, snapshot, restart-only keys) is
  readable without the HTTP surface around it.
- `error.rs` - `CompletionError`, `ProxyError` and `error_for`. The mapping from
  a failed upstream attempt to the client's status/code, including the side
  effect of a dead login, is now one file with one direction of dependency.

`main.rs` re-exports both, so no other module changed. What is left is the HTTP
surface: router, handlers, `LiveTurn` and the test module.

Tests: no test changed. `cargo test --workspace` is 203 + 37 green, and
`cargo clippy --workspace --all-targets` reports the same 28 warnings as before
the split (none from the new modules).

Accepted cost: two more files to open when following one type, and `main.rs`
still owns the streaming half. This is the first cut, not the last - the next
natural boundary is the streaming turn itself, which stays out of scope here
because a behavioural refactor of `stream_response` would not be reviewable in
the same change.

---

## 2026-10-31 - The log level in config.toml works; failed tool calls name the tool

**`DEEPSEEK_LOG="debug"` in `config.toml` did nothing.** The line sat under
`[limits]`, so it was an unknown key, and the log level was read only from the
environment. Debug output needed the variable set on every start. The level is
now `[proxy] log_level`, read before logging starts. `DEEPSEEK_LOG` still wins
when set, so one-off runs work as before.

**A failed tool call did not say which tool it was.** The `tool repair` WARN
gave reason codes only, and the raw reply was logged at debug only. The WARN now
names the tools too (`tools=`, arguments excluded). A turn that fails without a
repair logs the same line, so the tool shows with repair off as well.

Tests: `log_level_is_read_from_the_proxy_section`, `the_config_file_sets_the_profile_when_the_env_is_unset`,
`the_env_var_overrides_the_config_file`, `tool_names_are_logged_and_arguments_are_not`.

Accepted cost: the config file is read twice at startup, once for the log level
and again for the full config. A file that does not parse is reported by the
full load, as before.

---

## 2026-10-31 - `update` pulls and rebuilds, and stops there

**Getting a new build meant remembering four commands.** `git pull && cargo
build --release && deepseek-rs restart`, in the right directory, with the right
branch checked out. `deepseek-rs update` now does the first two: it finds the
checkout the running binary was compiled from (`CARGO_MANIFEST_DIR`, baked in at
build time), requires a clean tree on `main`/`master` with an upstream, runs
`git pull --ff-only`, then `cargo build --release`.

**It does not restart.** A build under a live server is harmless — cargo
renames the new binary into place — but restarting drops in-flight turns, and
picking up new code is a decision, not a side effect. The last line names the
pid that is still on the old binary and the command to fix that.

**The dirty-tree refusal is the point.** This repo routinely holds uncommitted
work (see AGENTS.md), and `--ff-only` would either fail confusingly or, with a
plain pull, produce a merge nobody asked for. `--force` is the escape hatch and
deliberately skips git entirely: build what is here, now. A build failure
leaves the previous binary in place and says so.

Tests: `a_clean_main_tree_may_pull`, `uncommitted_work_blocks_the_pull`,
`a_feature_branch_blocks_the_pull`, `detached_head_blocks_the_pull`,
`force_is_the_only_flag`.

Accepted cost: `update` assumes the source checkout still exists on this
machine. A binary copied elsewhere reports the missing directory instead of
guessing.

---

## 2026-10-31 - A call cut off after its last argument is accepted

The backend often stops after its last argument and skips the closers. The
arguments are whole, so the call is accepted, and the schema check still runs on
it (ADR-0018). A cut-off inside an argument still fails, because the value may be
partial. Of the 152 captured replies, 20 of the 24 with this shape now produce a
call instead of a repair turn.

Test: `a_cut_off_call_runs_only_if_its_arguments_are_complete`.

---

## 2026-10-31 - Native prompts may name the DSML format; the marker stays out

The owner clarified ADR-0014: the risk is the exact marker spelling and concrete
markup, not the word. Prompts may now name the format in words. The marker, the
fullwidth bar and any tag example stay forbidden (ADR-0017). The two tests that
banned the word "dsml" now ban the bar and angle brackets instead.

---

## 2026-10-31 - A call that drops its tool name runs when its arguments identify one tool

**The backend often sends a call's arguments with no tool block naming the tool.**
The parser had no name, so the reply went to repair, which costs a model turn and
resolved nothing in 137 of 284 settled attempts. Now the proxy runs the call if
exactly one offered tool's schema fits its argument names. Zero or several
matches stay malformed, as before (ADR-0016). Of the 152 captured replies, about
57 that used to need repair now produce a call. The probe used approximate
schemas, so re-check against the real ones.

A block that still cannot be run now gets the repair reason "the block that names
the tool had no name", not "an argument block appeared outside any tool block".
The model sees that wording in the repair turn.

Test: `a_nameless_reply_runs_the_one_tool_its_arguments_fit`.

---

## 2026-10-30 - A repair that drops the call delivers the answer

**A correction turn that answered without a call looped to failure.** The
model wrote a malformed call, the proxy asked for a repair, and the model read
its own call, decided it was not needed, and answered in prose. The repair loop
then carried the old failure forward, asked again, and got prose again until
`TOOL_REPAIR_MAX_RETRIES` ran out and the turn failed with `tool_repair_failed`.
The prose was discarded throughout. Now a repair reply with no call, on an
optional `tool_choice`, drops the failure and delivers its prose. Under
`tool_choice: "required"` a call is still owed, so the repair continues as
before. Tests: `a_repair_that_drops_the_call_delivers_its_answer`,
`a_repair_that_still_carries_a_call_is_not_declined`,
`a_required_call_is_never_declined`.

Accepted risk: a model that drops a call it actually meant to make now gets its
prose delivered instead of a second chance at the call.

---

## 2026-10-30 - A throttled tool repair waits before it retries

**The repair loop fired all its attempts back to back.** The backend reports a
throttle as a 200 whose body carries "Messages too frequent", so
`completion_with_retry` never sees it and the loop had no delay. A real log
burned all five `TOOL_REPAIR_MAX_RETRIES` attempts in about two seconds and then
failed the turn with `tool_repair_failed`. Each throttled repair now waits the
same linear backoff the turn path uses (`backoff_delay`), and the final attempt
does not wait. Test: `a_throttled_repair_waits_before_the_next_attempt`.

Accepted cost: a turn that is throttled through every repair can now take
roughly `RATE_LIMIT_BACKOFF_S * (1 + 2 + … + (n − 1))` before it fails. With the
shipped config (`backoff 10s`, 5 repairs) that is up to about 100 seconds.

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

**Fixed: repair told the model to stop using DSML and write `<tool>` instead.**
`dialect_dsml::explanation` predated DSML becoming the official format. When a
DSML call failed to parse it said "That is NOT the syntax for this conversation
... write the call as `<tool>...`", which taught the legacy tag in native mode
and is the likely reason models drifted to `<tool>`. It now says only what went
wrong and to resend in the native format. `native_repair_prompts_never_spell_out_markup`
now covers the unparsable path too.

Known gap left open: `deepseek-rs rebind --yes` without `--all` or a thread
silently stays a dry run.

*Files:* `config.toml`, `crates/proxy/src/{main,dialect,pool,session,store,threads}.rs`,
`crates/proxy/tests/http.rs`
