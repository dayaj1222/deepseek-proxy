# Documentation

The documents here answer three different questions. Read the one that matches
yours.

| If you want to know… | Read |
|---|---|
| what the server does today, and how to call it | [`../README.md`](../README.md) |
| what changed, when, and **why** | [`CHANGELOG.md`](CHANGELOG.md) |
| why a particular design is the way it is | [`adr/`](adr/) — one decision per file |
| what is planned next, in detail | [`features/`](features/) — one feature per file |
| what the model actually receives, and why it is wrong | [`audit/system-prompt-audit.md`](audit/system-prompt-audit.md) |
| the task list and ground rules for contributors | [`../PLAN.md`](../PLAN.md) |
| the vocabulary used everywhere | [`../GLOSSARY.md`](../GLOSSARY.md) |

## Convention

- **CHANGELOG** is append-only and written per landed change, not per release.
  Entries state the *reason*, not only the diff, because the diff is in git.
- **ADRs** are numbered, immutable once accepted, and superseded rather than
  edited. `0001` predates this convention.
- **Feature plans** are living documents until the feature lands; then the plan
  is frozen and the CHANGELOG entry becomes the record.

## Index

### Decisions

| ADR | Decision |
|---|---|
| [0001](adr/0001-unterminated-tool-block-is-malformed.md) | An unterminated tool block is malformed, never prose |
| [0002](adr/0002-one-chunk-model-for-both-transports.md) | One chunk model feeds both SSE and non-streaming JSON |
| [0003](adr/0003-openai-error-envelope-everywhere.md) | Every error, including 401/404/405, uses the OpenAI envelope |
| [0004](adr/0004-request-id-in-a-tracing-span.md) | `x-request-id` is minted outermost and lives in a tracing span |
| [0005](adr/0005-turn-snapshots-settings.md) | A turn snapshots settings once; reload never moves a deadline |
| [0006](adr/0006-pool-is-swapped-not-mutated.md) | The account pool is swapped behind an `Arc`, never mutated in place |
| [0007](adr/0007-breaker-outside-the-rate-gate.md) | Breaker state is not behind the rate-gate mutex |
| [0008](adr/0008-rewind-before-rebuild.md) | A stale cursor is rewound before the session is rebuilt |
| [0009](adr/0009-cors-is-open.md) | CORS is open; the bearer key is the authorization boundary |
| [0010](adr/0010-shutdown-is-bounded.md) | Shutdown is bounded: readiness first, then a grace window |
| [0011](adr/0011-swallow-stray-closing-tags.md) | Up to two stray closing tags after a block are swallowed |
| [0012](adr/0012-501-for-unsupported-endpoints.md) | Unsupported OpenAI endpoints answer 501, never a fake success |

### Feature plans

| Task | Feature | Status |
|---|---|---|
| T-04 | API-key metadata: label, expiry, per-key model allowlist, RPM | planned |
| T-11 | Reasoning token count in `usage` (rest of T-11) | partial |
| T-05 | Usage accounting and per-key rate limit | planned |
| T-06 | Prometheus `/metrics` | planned |
| T-08 | `max_tokens`, `max_completion_tokens`, `stop` | planned |
| T-09 | Accept-and-ignore sampling params, with visibility | planned |
| T-10 | `response_format` (`json_object`, `json_schema`) | planned |
| T-12 | `n > 1` | planned |
| T-13 | Multiple images per turn | planned |
| T-14 | `/v1/responses` | planned |
| T-16 | Debug capture and replay | planned |
| T-17 | Deployment artifacts | planned |
| T-18 | Optional TLS and open-exposure guard | planned |
