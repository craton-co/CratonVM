#!/bin/bash
# selector-eintr-probe.sh <exe|hotspot> <tag> [iterations] [timeout_ms]
#
# Runs `probes/SelectorEintrProbe.java` while hammering EVERY thread of the
# process with SIGCONT, so the thread parked in `epoll_wait` is guaranteed to
# take a signal. `epoll_wait` and `poll` are NOT restarted by SA_RESTART — they
# always surface EINTR — so this is a deterministic way to ask whether
# `Selector.select(timeout)` honours its deadline across a signal.
#
# SIGCONT on an already-running process has no effect other than the EINTR it
# forces, which is why it is the signal used here.
#
# Score: `premature=0` is correct behaviour (the JDK's). Anything above 0 is
# the defect Netty reports as "Selector.select() returned prematurely 512 times
# in a row; rebuilding Selector".
set -u
EXE="${1:?usage: selector-eintr-probe.sh <exe|hotspot> <tag> [iters] [timeout_ms]}"
TAG="${2:-probe}"
ITERS="${3:-20}"
TMO="${4:-1000}"
JDK="${JDK:-/data/jdk25-real-20260717/jdk-25.0.3+9}"
CLASSES="${CLASSES:-/tmp/selprobe}"
OUT="/tmp/sel-$TAG.log"

if [ ! -f "$CLASSES/SelectorEintrProbe.class" ]; then
  echo "missing $CLASSES/SelectorEintrProbe.class — compile it first:" >&2
  echo "  $JDK/bin/javac -d $CLASSES probes/SelectorEintrProbe.java" >&2
  exit 2
fi

if [ "$EXE" = "hotspot" ]; then
  "$JDK/bin/java" -cp "$CLASSES" SelectorEintrProbe "$ITERS" "$TMO" > "$OUT" 2>&1 &
else
  "$EXE" --java-home "$JDK" --Xmx 1g -cp "$CLASSES" SelectorEintrProbe "$ITERS" "$TMO" > "$OUT" 2>&1 &
fi
PID=$!

# Signal storm: every thread, every 2 ms, for as long as the probe runs.
(
  while kill -0 "$PID" 2>/dev/null; do
    for t in /proc/$PID/task/*; do
      kill -CONT "$(basename "$t")" 2>/dev/null
    done
    sleep 0.002
  done
) &
STORM=$!

wait "$PID"; RC=$?
kill "$STORM" 2>/dev/null; wait "$STORM" 2>/dev/null

echo "[$TAG] rc=$RC $(grep -o 'SELECT_EINTR_PROBE_RESULT.*' "$OUT" || echo 'NO RESULT')"
