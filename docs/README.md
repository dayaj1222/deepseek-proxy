# Documentation

History before 2026-10-28 (the old plan, per-feature notes, prompt audit and
changelog) was removed deliberately; it is all in git (`git log -- PLAN.md docs/`).
The ADRs were kept because a recorded decision is the one thing a diff cannot
give back.

| If you want to know… | Read |
|---|---|
| what the server does and how to call it | [`../README.md`](../README.md) |
| the rules every contributor and AI agent follows | [`../AGENTS.md`](../AGENTS.md) |
| what changed, and **why** | [`CHANGELOG.md`](CHANGELOG.md) |
| why a design is the way it is | [`adr/`](adr/) — one decision per file |
| the vocabulary | [`../GLOSSARY.md`](../GLOSSARY.md) |

## Convention

- **CHANGELOG** is append-only, newest first, one entry per landed change. State
  the reason; the diff is in git.
- **ADRs** are numbered and immutable once accepted. To change a decision write
  a new ADR that supersedes it. Start from [`adr/TEMPLATE.md`](adr/TEMPLATE.md)
  and always record the alternatives rejected and the risk accepted.

## ADR index

| ADR | Decision |
|---|---|
| [0001](adr/0001-unterminated-tool-block-is-malformed.md) | An unterminated tool block is malformed, never prose |
| [0002](adr/0002-one-chunk-model-for-both-transports.md) | One chunk model feeds both SSE and non-streaming JSON |
| [0003](adr/0003-openai-error-envelope-everywhere.md) | Every error uses the OpenAI envelope |
| [0004](adr/0004-request-id-in-a-tracing-span.md) | `x-request-id` is minted outermost and lives in a tracing span |
| [0005](adr/0005-turn-snapshots-settings.md) | A turn snapshots settings once; reload never moves a deadline |
| [0006](adr/0006-pool-is-swapped-not-mutated.md) | The account pool is swapped behind an `Arc`, never mutated |
| [0007](adr/0007-breaker-outside-the-rate-gate.md) | Breaker state is not behind the rate-gate mutex |
| [0008](adr/0008-rewind-before-rebuild.md) | A stale cursor is rewound before the session is rebuilt |
| [0009](adr/0009-cors-is-open.md) | CORS is open; the bearer key is the authorization boundary (amended by 0015) |
| [0010](adr/0010-shutdown-is-bounded.md) | Shutdown is bounded: readiness first, then a grace window |
| [0011](adr/0011-swallow-stray-closing-tags.md) | Up to two stray closing tags after a block are swallowed |
| [0012](adr/0012-501-for-unsupported-endpoints.md) | Unsupported OpenAI endpoints answer 501 |
| [0013](adr/0013-one-interface-for-thread-identity.md) | One interface opens every remote session |
| [0014](adr/0014-native-dialect-teaches-no-wire-format.md) | The default dialect teaches no tool wire format (on trial) |
| [0015](adr/0015-admin-routes-take-no-key.md) | The admin routes take no API key |
