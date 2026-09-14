#!/bin/bash
# native-registration-census.sh <cratonvm> <outdir> [java-home]
#
# One strict-mode boot per SCHEDULED regression vector, each writing its own
# `--dump-native-registry` JSON, so per-triple `invocations` can be UNIONED
# across the corpus rather than read off a single program.
#
# # Why this is not `run.sh --jdk-only-report`
#
# `run.sh` already writes a per-vector `--jdk-only-report`, and that is a
# DIFFERENT instrument: it records dispatch OBSERVATIONS (a native was reached
# where the real method has `Code`), it is PID-scoped, and it is `rm -rf`'d at
# the end of the run. This one records the REGISTRY -- every triple, its owner,
# its kind, its invocation count, and what the real image declares for it --
# which is what an adjudication needs, because a retirement acts on a
# registration. The two numbers do not agree and must not be quoted
# interchangeably; `WORKER-4-2` section 1 has the worked example.
#
# # Cost
#
# One VM boot per vector, ~6 MB of JSON each. On a warm host the whole corpus is
# a few minutes and ~700 MB. Delete `<outdir>` when the table is drawn.
#
# Uses the classes `regression-suite/run.sh` has already compiled into
# `regression-suite/build`; run the suite once first if that directory is empty.
set -u

VM="${1:?usage: native-registration-census.sh <cratonvm> <outdir> [java-home]}"
OUT="${2:?usage: native-registration-census.sh <cratonvm> <outdir> [java-home]}"
JH="${3:-${JAVA_HOME:?set JAVA_HOME or pass one}}"

HERE="$(cd "$(dirname "$0")/.." && pwd)"
CP="$HERE/regression-suite/build"
RUNSH="$HERE/regression-suite/run.sh"

[ -d "$CP" ] || { echo "no compiled vectors at $CP — run regression-suite/run.sh once first"; exit 2; }

# The two scheduled lists, read from run.sh rather than copied, so a vector
# added there is picked up here without a second edit.
CORE=$(grep -m1 -oP '(?<=^CORE_CLASSES=")[^"]+' "$RUNSH")
JDKONLY=$(grep -m1 -oP '(?<=^JDKONLY_CLASSES=")[^"]+' "$RUNSH")

rm -rf "$OUT"; mkdir -p "$OUT"
LOG="$OUT/census.log"; : > "$LOG"

n=0; skipped=0
for c in $CORE $JDKONLY; do
  if [ ! -f "$CP/$c.class" ]; then
    echo "skip $c (not compiled)" >> "$LOG"; skipped=$((skipped + 1)); continue
  fi
  timeout 300 "$VM" --jdk-only --java-home "$JH" \
      --dump-native-registry "$OUT/$c.json" -cp "$CP" "$c" > /dev/null 2>&1
  echo "$c rc=$? size=$(stat -c%s "$OUT/$c.json" 2>/dev/null || echo 0)" >> "$LOG"
  n=$((n + 1))
done

# A vector that produced NO dump is a hole in the union, and a silent hole reads
# as "nothing registered there". Name them.
empty=$(awk '$3 == "size=0" { print $1 }' "$LOG" | tr '\n' ' ')
echo "census: $n vectors run, $skipped not compiled" | tee -a "$LOG"
[ -n "$empty" ] && echo "census: NO DUMP from: $empty" | tee -a "$LOG"
echo "DONE" >> "$LOG"
echo "now: python3 scripts/native-registration-adjudication.py $OUT <class-prefix>"
