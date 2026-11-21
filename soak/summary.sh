#!/usr/bin/env bash
# Summarise the logs of a soak run: soak/summary.sh ARENA
#
# For each agent: how many calls it made, how many typed, the exit statuses,
# the slowest call, and the hints it was given. Read this beside the agents'
# reports: a report is model output, the log is what happened.
set -eu
arena=${1:?usage: soak/summary.sh ARENA}

for log in "$arena"/*/log; do
  [ -f "$log" ] || continue
  agent=$(basename "$(dirname "$log")")
  calls=$(grep -c '^=== ' "$log" || true)
  typed=$(cat "$(dirname "$log")/count" 2>/dev/null || echo 0)
  slowest=$(grep '^=== ' "$log" | sed 's/.* ms=\([0-9]*\) .*/\1/' | sort -n | tail -1)
  exits=$(grep '^=== ' "$log" | sed 's/.* exit=\([0-9]*\) .*/\1/' | sort -n | uniq -c | awk '{printf "%s x%s  ", $2, $1}')
  echo "$agent: $calls calls, $typed typing; slowest ${slowest:-0} ms; exit statuses: $exits"
  grep '^hint: ' "$log" | cut -c1-100 | sort | uniq -c | sed 's/^/    /' || true
done

echo
echo "calls slower than 5 s:"
grep -H '^=== ' "$arena"/*/log | awk '{ for (i = 1; i <= NF; i++) if ($i ~ /^ms=/) { split($i, a, "="); if (a[2] > 5000) print "  " substr($0, 1, 200) } }' || true
