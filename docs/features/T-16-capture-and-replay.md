# T-16 - Debug capture and replay

**Status:** planned · **Effort:** M · **Depends on:** nothing
**Roadmap entry:** [`PLAN.md`](../../PLAN.md) Phase D

## Problem

A field bug arrives as "the model's tool call came out wrong". Reproducing it
needs the exact upstream bytes, which are gone. The `corpus/` fixtures exist
because someone once captured streams by hand; that should be a switch, not a
task.

## Surface

Off by default. Never on in `serve` unless explicitly configured.

```toml
[debug]
capture_dir      = "/var/lib/deepseek-rs/capture"  # unset = off
capture_full     = false     # false = truncate message text to N chars
capture_max_files = 500      # ring: oldest evicted
```

### One JSON file per turn

```json
{
  "request":  { "model": "...", "stream": true, "messages": "[truncated]" },
  "upstream": { "sse": "<raw bytes, or a path to them>" },
  "timings":  { "prepare_ms": 3, "first_byte_ms": 410, "total_ms": 2210 },
  "decisions":{ "retries": 0, "repairs": 1, "failover": null, "rewind": false },
  "usage":    { "prompt_tokens": 900, "completion_tokens": 120 }
}
```

**Redaction is the feature, not a detail.** No `Authorization` header, no
password, no login token, no API key. Messages are truncated unless
`capture_full = true`, and that flag is called out in the README as "this writes
user content to disk". Files are `0600`, directory `0700`.

### Replay

```
deepseek-rs replay <file>
```

Feeds the captured SSE through `protocol::FragmentRouter` and prints the routed
output. Enough to turn a field bug into a new `corpus/` fixture without guessing
which bytes mattered.

### Unknown-event counter

`protocol` counts SSE events it does not recognise; `/stats` reports it. Upstream
drift becomes visible before it becomes a bug report.

## Tests

- redaction: a request carrying a bearer token and a password produces a file
  containing neither string (grep the file, not the struct)
- truncation: message text is cut at N chars when `capture_full = false`
- ring eviction: 501 turns with `capture_max_files = 500` leaves 500 files and the
  oldest is gone
- replay of a `corpus/*.sse` file prints the expected routed output
- unknown-event counter increments on an unrecognised event
- permissions are 0600 on a unix host

## Not in scope

Replaying a capture against a live upstream, a capture viewer UI, capture of
non-turn requests.

## Risks

- **PII on disk** is the whole risk. Defaults are off, truncated, `0600`, and
  ring-bounded; the README says plainly what `capture_full` does. A redaction
  test asserts the *file contents*, because a redaction function that is not
  called is a false sense of safety.
- **Disk fill** on a busy server: the ring bounds it, and the size should also be
  bounded by a byte cap, not only a file count.
