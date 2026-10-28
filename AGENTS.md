# AGENTS.md — read this before touching the repo

Applies to every AI agent and human contributor. `CLAUDE.md` points here.
Keep this file short; it routes you to the documents that hold the detail.

## What this is

`deepseek-rs` is an OpenAI-compatible proxy in front of DeepSeek's web chat.
Crates: `proxy → client → protocol → pow` (dependencies point strictly
downward; nothing below `proxy` knows about HTTP serving or SQLite).

## Read first, in this order

1. [`GLOSSARY.md`](GLOSSARY.md) — use these words (thread, turn, delta, cursor,
   repair turn, account, surface). Do not invent synonyms.
2. [`docs/README.md`](docs/README.md) — which document answers which question.
3. The ADRs for the area you are about to change: `ls docs/adr`. A decision
   recorded there is not up for re-litigation inside an unrelated task.

## Hard constraints (the ones that cost us before)

- **Never teach a tool wire format.** The default `native` dialect names no
  syntax; the model uses its own and we read it back. Teaching the backend's
  tag format is what risks an account strike. Do not add wording, examples or
  repair text that spells out a concrete format in `native` mode.
  See [ADR-0014](docs/adr/0014-native-dialect-teaches-no-wire-format.md).
- **One path opens a remote session:** `session::open_session`. Never build a
  `ThreadState` by hand. See ADR-0013.
- **Do not hit the real DeepSeek in automated tests.** Use `corpus/` fixtures or
  the mock upstream. Real-account checks are manual (`deepseek-rs check`).
- **Never log secrets, tokens, passwords or full prompts.**
- **SQLite migrations are append-only** (`Store::migrate`). Never edit or drop a
  column. Add a test that opens a DB made by the previous schema.
- **Compatibility:** never change a request/response shape in a way that breaks
  a working OpenAI client. New fields are additive; new config keys default to
  the old behaviour.
- **Optional dialects** (`json_invoke`, `simple_tool_tag`) stay supported and
  selectable. Do not delete them; do not make them the default.

## Working agreement

1. `git status` first. The tree may hold someone's uncommitted work. Do not
   stash, reset or reformat files you do not own.
2. Baseline must be green before you start:
   `cargo build --workspace && cargo test --workspace`. If it is not, stop and
   report; do not fix unrelated failures inside your task.
3. One task = one branch (`feat/<slug>`) = one reviewable change. Never
   mix tasks. Commit messages: `type(scope): summary` (`feat`, `fix`,
   `refactor`, `docs`, `chore`, `test`).
4. Match the surrounding code: comment density, naming, error idiom.
5. Definition of done: build + tests pass, `cargo clippy --workspace
   --all-targets` adds no warnings, new behaviour has tests, a bug fix has a
   regression test that fails without the fix.
6. Surface changes (endpoint, CLI, config key, env var) update `README.md`.
   A config key lives in three places: `config.rs`, the `config.toml` example,
   and the env-override list.
7. Report honestly in the commit/PR body: what you verified, what you did not,
   and any assumption you made.

## The decision trail (do this, it is part of the change)

| You did this | Write this |
|---|---|
| Landed any behaviour change | a [`docs/CHANGELOG.md`](docs/CHANGELOG.md) entry: the **reason**, not the diff |
| Chose between real alternatives, or reversed an earlier choice | an ADR from [`docs/adr/TEMPLATE.md`](docs/adr/TEMPLATE.md); add it to the index in `docs/README.md` |
| Added or renamed a domain concept | `GLOSSARY.md` |

Rules for ADRs: numbered, immutable once accepted — to change a decision, write
a new ADR that supersedes it and mark the old one `superseded by ADR-nnnn`.
Record the **alternatives you rejected** and the **risk you accepted**; that is
the part nobody can reconstruct from the diff later.

If the user overrides a safeguard (e.g. removes an auth check), do it, and
record it as an ADR with the accepted risk. See
[ADR-0015](docs/adr/0015-admin-routes-take-no-key.md).

## Handy commands

    cargo build --release                # ~/.local/bin/deepseek-rs symlinks to target/release
    deepseek-rs restart                  # picks up the new binary and config
    deepseek-rs rebind --all --yes       # move flagged threads (bare --yes stays a dry run)
    cargo test --workspace
