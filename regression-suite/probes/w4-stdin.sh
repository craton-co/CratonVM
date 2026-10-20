#!/bin/bash
# w4-stdin.sh <cratonvm> [java-home] [workdir]
#
# `W4Stdin.java` against the HotSpot oracle, WITH STDIN FED.
#
# # Why this needs a runner of its own
#
# Every other probe in this family is a plain `java`/`cratonvm` invocation. This
# one is not, for two reasons that are easy to get wrong and that make a run
# meaningless when they are:
#
#   1. **stdin must actually carry data.** With an empty stream, "this VM reads
#      nothing" and "this VM works" are the SAME observation, and that is how
#      `System.in`'s shape went unmeasured — see `WORKER-4-NOTE-4`.
#   2. **one mode per process.** stdin is consumed by whichever reader goes
#      first, so `raw`, `buffered`, `scanner`, `scannerBuffered` and `reader`
#      cannot share a run. Each gets its own process with its own feed.
#
# Diff the three sections per mode. The identity lines (`class`, `isBuffered`,
# `isFileInputStream`, `markSupported`) are the open divergence
# (`WORKER-4-NOTE-4`); every `read`/`next`/`line` value must match exactly.
set -u

VM="${1:?usage: w4-stdin.sh <cratonvm> [java-home] [workdir]}"
JH="${2:-${JAVA_HOME:?set JAVA_HOME or pass one}}"
WORK="${3:-$(mktemp -d)}"

HERE="$(cd "$(dirname "$0")" && pwd)"
mkdir -p "$WORK"

"$JH/bin/javac" -nowarn -d "$WORK" "$HERE/W4Stdin.java" || { echo "javac failed"; exit 1; }

# Three lines, so `nextLine` and `readLine` have something to be wrong about.
FEED=$'alpha beta\nsecond line\nthird\n'

for mode in raw buffered scanner scannerBuffered reader; do
  echo "=== ORACLE $mode ==="
  printf '%s' "$FEED" | timeout 60 "$JH/bin/java" -cp "$WORK" W4Stdin "$mode" 2>&1
  echo "oracle-$mode rc=$?"
  for m in --jdk-only --real-jdk; do
    echo "=== CRATONVM $m $mode ==="
    printf '%s' "$FEED" | timeout 120 "$VM" "$m" --java-home "$JH" -cp "$WORK" W4Stdin "$mode" 2>/dev/null
    echo "cratonvm $m $mode rc=$?"
  done
done
echo DONE
