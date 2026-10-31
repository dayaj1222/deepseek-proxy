# Malformed tool-call statistics

Baseline for the parser-leniency work. Counts come from the proxy's own logs,
not from chat exports. The raw replies contain user code and paths, so they stay
out of the repo. Regenerate the numbers with `malformed_stats.sh` (next to this
file) against a local copy of the raw corpus.

This report has two captures. The first (2026-10-30) is the original baseline.
The second (2026-10-31) was added later and is kept separately in the corpus
directory, so each figure can be traced to its capture.

## Sources

| Source | Lines | Notes |
|---|---|---|
| `~/.local/state/deepseek-rs/deepseek-rs.log` | 5,161 (after ANSI strip) | Appended by several background runs, 2026-10-18 to 2026-10-30. Only the Debug-profile run of 2026-10-29 (about 15:00 to 16:18) logged raw replies. |
| `~/deepseek-rs-debug.log`, 2026-10-30 capture | foreground run | Started with `DEEPSEEK_LOG=debug`. Raw replies from about 14:50 to 16:51. Saved as `src_debug.log`. |
| `~/deepseek-rs-debug.log`, 2026-10-31 capture | 4,438 (after ANSI strip) | Replaced the 2026-10-30 file in place. Log lines carry no date; times run 04:17 to 06:50, file modified 2026-10-31 12:19. Saved as `src_debug_2026-10-31.log`. |

All sources have ANSI colour codes in parts of the file. The extraction strips
them first (`sed 's/\x1b\[[0-9;]*m//g'`), because the colour codes split the
literal strings the search depends on.

No line is shared between the sources. The 2026-10-31 raw replies, repair
attempts and settled lines were each checked against the state log and the
2026-10-30 debug log, and against the existing corpus, with zero matches. The
counts below are therefore additive.

The corpus is `~/.local/state/deepseek-rs/malformed/`:

- `raw_replies.log` (78, 2026-10-30), with `repair_attempts.log` and `repair_settled.log`
- `raw_replies_2026-10-31.log` (74, 2026-10-31), with `repair_attempts_2026-10-31.log` and `repair_settled_2026-10-31.log`

## Volume

| | 2026-10-30 | 2026-10-31 | Combined |
|---|---|---|---|
| Malformed raw replies | 78 | 74 | **152** |
| Repair attempts (WARN `tool repair`) | 370 | 115 | **485** |
| Attempts with a reason code | 185 | 115 | **300** |
| Attempts without a reason code | 185 | 0 | **185** |
| Threads with attempts | 15 | 2 | **17** |
| Repair settled lines | 177 | 107 | **284** |

The 2026-10-31 capture logs a reason code on every attempt. The 185 attempts
without one are all from the 2026-10-30 capture, so their failure type is still
unknown.

## Structure of the raw replies (per reply, exclusive)

| Class | 2026-10-30 | 2026-10-31 | Combined |
|---|---|---|---|
| A. No invoke tag at all | 16 | 58 | **74** |
| B. Invoke opened and closed | 4 | 4 | **8** |
| C. Invoke opened, never closed | 58 | 12 | **70** |
| Wrapper (`calls`) never closed | 16 | 8 | **24** |

Class A is now the largest group, and most of its new members have a different
shape from the old ones (see below).

## Tag occurrences

| Form | 2026-10-30 | 2026-10-31 | Combined |
|---|---|---|---|
| Bare closer `</DSML>` (no element word) | 153 | 182 | **335** |
| Named `</DSML parameter>` | 53 | 19 | **72** |
| Named `</DSML invoke>` | 14 | 46 | **60** |
| `string="false"` parameters | 36 | 32 | **68** |
| `string="true"` parameters | 51 | 46 | **97** |
| Bare opener `<DSML>` (no element word) | 4 (in 3 replies) | 8 (in 6 replies) | **12 (in 9 replies)** |
| Parameters with no invoke tag (replies) | 15 | 49 | **64** |

A bare closer is used both for the parameter closer and for the invoke closer.
The parser already accepts bare closers in some positions.

## Shapes new in the 2026-10-31 capture

- **Parameters with no invoke tag.** 49 of the 58 class-A replies open `calls`
  and then emit `parameter` tags directly, with no `invoke` tag. The tool name is
  absent, so the call cannot be recovered by name. Nine further replies have only
  bare tags and no parameters at all.
- **Bare opener.** 8 `<DSML>` openers with no element word appear in 6 replies.
  The old corpus had 4 in 3 replies. Whether the parser reads them as closers or
  as errors has not been checked.

## Tool named in the invoke tag

| Tool | 2026-10-30 | 2026-10-31 | Combined |
|---|---|---|---|
| bash | 31 | 6 | **37** |
| edit | 14 | 6 | **20** |
| read | 8 | 3 | **11** |
| ls | 5 | 0 | **5** |
| codemode | 2 | 1 | **3** |
| memory_write | 1 | 0 | **1** |
| find | 1 | 0 | **1** |
| bg_run | 0 | 1 | **1** |

Only the tool names are recorded, not the arguments.

## Reason codes (repair attempts with a code)

| Code | 2026-10-30 | 2026-10-31 | Combined |
|---|---|---|---|
| `dsml_unparsable` | 169 | 89 | **258** |
| `dsml_unparsable,dsml_unparsable` | 9 | 12 | **21** |
| `dsml_unparsable` x3 | 0 | 4 | **4** |
| `dsml_unparsable` x4 | 0 | 4 | **4** |
| `arguments_do_not_match_schema` | 6 | 6 | **12** |
| `arguments_do_not_match_schema,dsml_unparsable` | 1 | 0 | **1** |

## Repair outcomes (settled lines)

| Outcome | 2026-10-30 (177) | 2026-10-31 (107) | Combined (284) |
|---|---|---|---|
| Repair resolved at least one call | 63 | 84 | **147** |
| Repair resolved nothing | 114 | 23 | **137** |

Settled lines in the 2026-10-30 capture exist only for thread `bddaaf96`. The
2026-10-31 capture has settled lines for two threads: `b3ad17f3` (94) and
`ae3f61ab` (13). Its 115 attempts and 107 settled lines leave 8 attempts with no
settled line, which has not been checked.

## Excluded: runaway replies

Seven replies from the 2026-10-31 capture were excluded as a one-off mistake.
Each one repeats the bare closer `</｜｜DSML｜｜>` between 3,926 and 4,091
times, about 53 KB per log line. They account for 28,375 of the 28,557 bare
closers in that capture. Six were in thread `b3ad17f3` and one in `ae3f61ab`.
The exclusion applies only to raw-reply statistics. Their repair attempts are
still counted, because they are real repair events.

## Caveats

- Raw bytes exist for three threads, so the structure figures describe three
  model sessions, not all traffic.
- Thread `41d08b45` has 6 attempt lines (5 with `resolved=0`, 1 with
  `resolved=1`) and no raw replies, so its failures cannot be classified.
- The 185 attempts from the 2026-10-30 capture carry no reason code, so their
  failure type is unknown.
- The sample is still small (152 replies). The 2026-10-31 capture adds two
  threads to the one from 2026-10-30.
- The seven runaway replies were excluded by hand, not by a rule in the script.
  The exclusion is the one-off decision recorded above.

## Reproducing

```
docs/analysis/malformed_stats.sh ~/.local/state/deepseek-rs/malformed/raw_replies.log
docs/analysis/malformed_stats.sh ~/.local/state/deepseek-rs/malformed/raw_replies_2026-10-31.log
```

To rebuild the 2026-10-30 corpus from the logs:

```
sed 's/\x1b\[[0-9;]*m//g' ~/.local/state/deepseek-rs/deepseek-rs.log > src_state.log
sed 's/\x1b\[[0-9;]*m//g' ~/deepseek-rs-debug.log > src_debug.log
grep -h "malformed tool call: raw reply" src_state.log src_debug.log > raw_replies.log
grep -h "tool repair thread=" src_state.log src_debug.log > repair_attempts.log
grep -h "tool repair settled" src_state.log src_debug.log > repair_settled.log
```

Do not run that against the current `~/deepseek-rs-debug.log`. It now holds the
2026-10-31 capture, so use `src_debug_2026-10-31.log` for that one. To rebuild the
2026-10-31 corpus, drop the runaway replies (more than 1,000 bare closers):

```
awk '{n=gsub(/<\/｜｜DSML｜｜>/,"&"); if(n<1000) print}' raw_replies_all_2026-10-31.log > raw_replies_2026-10-31.log
```

Do not deduplicate. The corpus has no duplicate lines, and dropping any would
change the counts.
