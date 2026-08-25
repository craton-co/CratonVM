#!/bin/bash
# triage.sh <dir> — separate a REAL stall from a merely SLOW run.
#
# `--stack-dump-on-timeout` fires on a fixed deadline, so on a loaded host it
# also fires on a run that was still making progress. Three columns decide it,
# and all three are already in the log:
#
#   waited_ms  the longest `Object.wait()` the census found. A stall is
#              hundreds of thousands; a healthy mid-test wait is single digits.
#   STUCK      the Java-side probe's report, which fires only after ONE netty
#              operation has been outstanding for 15 s.
#   progress   whether a test finished within the last stretch of the log.
set -u
D="$1"
printf "%-14s %-6s %-6s %-10s %-7s %-9s %s\n" run status wall waited_ms stuck waiters inflight
for L in "$D"/run-*.log "$D"/CVM-*.log; do
  [ -f "$L" ] || continue
  n=$(basename "$L" .log)
  wm=$(grep -ao "waited_ms=[0-9]*" "$L" | sed 's/waited_ms=//' | sort -n | tail -1)
  stuck=$(grep -ac "STUCK " "$L")
  cens=$(grep -ac "\[WAIT-CENSUS\] tid=" "$L")
  inv=$(grep -a "@@BEGIN" "$L" | tail -1 | sed 's/.*test-template:\([a-zA-Z]*\)(.*invocation:/\1 /;s/\]//')
  dump=$(grep -ac "watchdog: deadline" "$L")
  st=PASS; [ "$dump" -gt 0 ] && st=DUMPED
  printf "%-14s %-6s %-6s %-10s %-7s %-9s %s\n" "$n" "$st" "-" "${wm:--}" "$stuck" "$cens" "${inv:--}"
done
