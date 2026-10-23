# T-17 - Deployment artifacts

**Status:** planned · **Effort:** S · **Depends on:** nothing (CI wants a clean clippy baseline)
**Roadmap entry:** [`PLAN.md`](../../PLAN.md) Phase E

## Problem

Today the answer to "how do I run this" is `cargo build --release` and a README
section. There is no unit file, no image, no CI, so every deployment rediscovers
the same hardening flags and the same health-check wiring - and nothing proves
the tree builds from scratch on a machine that is not this one.

## Deliverables

### `contrib/deepseek-rs.service`

systemd **user** unit:

- `Restart=on-failure`, `RestartSec=2`
- `ExecReload=/bin/kill -HUP $MAINPID` - matches T-15's reload
- hardening: `NoNewPrivileges=yes`, `ProtectSystem=strict`,
  `ProtectHome=read-only`, `PrivateTmp=yes`, `RestrictAddressFamilies=AF_INET
  AF_INET6 AF_UNIX`, and `ReadWritePaths=%S/deepseek-rs` for the SQLite file and
  the pidfile
- `Environment=CONFIG_PATH=%h/.config/deepseek-rs/config.toml`

Verify with `systemd-analyze verify`.

### `Dockerfile`

Multi-stage: builder on the pinned toolchain, final on `debian-slim` (or
`distroless/cc`), non-root `nobody`, `HEALTHCHECK` on `/healthz`,
`STOPSIGNAL SIGTERM` so ADR-0010's drain actually happens, and a `VOLUME` for the
database. `docker build .` must succeed in CI, not only locally.

### `PKGBUILD`

AUR packaging: build from the tagged tarball, install the binary, the unit and
the example config.

### CI (`.github/workflows/ci.yml`)

1. `cargo fmt --check`
2. `cargo clippy --workspace --all-targets -- -D warnings` - **only after the
   baseline is clean.** At the time of writing it is not (21 warnings), so the
   first commit fixes those and the `-D warnings` step lands after.
3. `cargo test --workspace`
4. `docker build`
5. release artifact on tag

### README

A "Deployment" section pointing at the three artifacts, not repeating them.

## Tests

- CI is the test: a fresh clone builds, lints and tests on a clean machine.
- `systemd-analyze verify contrib/deepseek-rs.service`
- `docker build .` and a container answering `/healthz`

## Not in scope

Kubernetes manifests, Helm charts, distro packages beyond AUR, cross-compilation.

## Risks

- **`-D warnings` blocks every PR** if the baseline is not clean first. Order the
  commits: fix warnings, then turn on the gate.
- **Hardening flags that break the server**: `ProtectSystem=strict` without the
  right `ReadWritePaths` makes the SQLite open fail at startup. That is what the
  verify step and a manual user-unit run are for.
