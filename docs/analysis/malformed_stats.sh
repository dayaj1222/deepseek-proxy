#!/usr/bin/env bash
# Structure statistics for malformed tool-call replies.
#
# Input: a file of "malformed tool call: raw reply" log lines (DEBUG level),
# one per line, as written by the proxy. Keep that file outside the repo: the
# replies contain user code and paths.
#
#   docs/analysis/malformed_stats.sh ~/.local/state/deepseek-rs/malformed/raw_replies.log
#
# Counts are per reply for the classes, per occurrence for the tags.
set -euo pipefail

raw="${1:?usage: $0 <raw_replies.log>}"

echo "replies: $(wc -l < "$raw")"
echo
echo "classes (per reply, exclusive):"
awk '
  {
    if (index($0, "｜｜DSML｜｜ invoke name=") == 0) a++
    else if (index($0, "</｜｜DSML｜｜ invoke>") > 0) b++
    else c++
    if (index($0, "</｜｜DSML｜｜ calls>") == 0) w++
  }
  END {
    printf "  A no invoke tag:                 %d\n", a
    printf "  B invoke opened and closed:      %d\n", b
    printf "  C invoke opened, never closed:   %d\n", c
    printf "  wrapper never closed:            %d\n", w
  }' "$raw"
echo
echo "occurrences:"
printf "  bare closer </DSML>:             %s\n" "$(grep -o '</｜｜DSML｜｜>' "$raw" | wc -l)"
printf "  named </DSML parameter>:         %s\n" "$(grep -o '</｜｜DSML｜｜ parameter>' "$raw" | wc -l)"
printf "  named </DSML invoke>:            %s\n" "$(grep -o '</｜｜DSML｜｜ invoke>' "$raw" | wc -l)"
printf "  string=\"false\" parameters:        %s\n" "$(grep -o 'string=\\\?"false' "$raw" | wc -l)"
printf "  string=\"true\" parameters:         %s\n" "$(grep -o 'string=\\\?"true' "$raw" | wc -l)"
printf "  bare opener <DSML> (no word):    %s\n" "$(grep -o '<｜｜DSML｜｜>' "$raw" | wc -l)"
printf "  replies with parameters, no invoke: %s\n" "$(awk 'index($0, "｜｜DSML｜｜ invoke name=") == 0 && index($0, "DSML｜｜ parameter") > 0' "$raw" | wc -l)"
echo
echo "tool named in invoke:"
grep -o 'invoke name=\\\?"[a-zA-Z_]*' "$raw" | sed 's/.*"//' | sort | uniq -c | sort -rn
