# T-06 - Prometheus `/metrics`

**Status:** planned · **Effort:** M · **Depends on:** nothing (better after T-05)
**Roadmap entry:** [`PLAN.md`](../../PLAN.md) Phase D

## Problem

`/stats` is a JSON snapshot for a human. Nothing scrapes it, so there is no
time series: a breaker that opened and closed between two `curl`s is invisible,
and there is no histogram of latency at all.

## Surface

`GET /metrics`, Prometheus text format, behind the same auth as `/stats` (or a
separate `[proxy] metrics_token` when the auth story is per-key, so a scraper
never needs a client key).

### Metric set

| Metric | Type | Labels |
|---|---|---|
| `dsrs_requests_total` | counter | `status`, `stream` |
| `dsrs_request_seconds` | histogram | `stream` |
| `dsrs_queue_waiting` | gauge | `account` |
| `dsrs_account_healthy` | gauge | `account` |
| `dsrs_breaker_state` | gauge | `account` (0 closed, 1 half-open, 2 open) |
| `dsrs_pow_seconds` | histogram | - |
| `dsrs_repair_turns_total` | counter | `outcome` |
| `dsrs_failovers_total` | counter | `reason` |
| `dsrs_tokens_total` | counter | `kind` (prompt, completion, reasoning) |
| `dsrs_images_dropped_total` | counter | - |

## Design

**No account email in a label.** Labels are a cardinality budget and an email is
PII. Use a stable short hash of the email (`acct-3f2a`) derived once at pool
build; `/stats` can carry the mapping because it is authenticated and read by a
human.

**Hand-rolled text output over the `prometheus` crate** unless that crate pulls
in fewer than 5 transitive dependencies. The format is four lines of code per
metric family; a dependency that dominates the binary for it is a bad trade.
Counters are `AtomicU64`; the histogram is a fixed bucket array behind a `Mutex`
(nothing in the critical section awaits).

**Same numbers as `/stats`.** Both read one metrics struct; if a value is in one
and not the other, that is a bug, not a feature.

## Tests

- scrape after one mock request: `dsrs_requests_total{status="200",stream="false"} 1`
- counters are monotonic across two scrapes
- breaker gauge reflects an open breaker (reuse the T-03 mock flap)
- no email appears anywhere in the body (grep for `@`)
- histogram `_bucket`/`_sum`/`_count` lines are well-formed for a real scrape parser

## Not in scope

OTLP, tracing export, Grafana dashboards (a `contrib/` example may come with T-17).

## Risks

- **Cardinality explosion** if someone labels by `thread`. The rule is in the
table above and enforced by review; only the listed labels exist.
- **Scrape cost** on a large pool: the gather walks accounts per scrape. At the
  pool sizes this server targets that is microseconds; if it ever is not, cache
  the rendered body for one second.
