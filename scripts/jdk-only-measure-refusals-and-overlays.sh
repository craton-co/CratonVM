#!/bin/bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
#
# Two jdk-only measurements whose instruments already existed and which nobody
# had run. Both overturned a claim in docs/known-issues/jdk-only/ on 2026-08-04.
#
#  A. Inline-cache native refusals (item 11 section 1).
#     That record says the strict-mode blanket refusal "costs JdkOnly runs
#     every inline-cached native call". That is a claim about a rate, and
#     --jdk-only-report has carried the counter all along:
#     refusals.jit_inline_cache_natives. Measured: 0, on every workload
#     including a deliberately JIT-hot one. Every MIC/PIC publication in
#     vm/src/jit/helpers.rs takes its entry from try_jit_compile_callee, which
#     is owned, so jit_entry_publishable returns early and the refusal is
#     unreachable from those sites.
#
#  B. Overlay-corruption census (item 2 step 1).
#     That record scopes step 1 as "sweep four crates by hand". The runtime
#     detector for the same defect already exists: CRATONVM_DBG_OVERLAY reports
#     a native writing a primitive into a reference slot, or a reference into a
#     primitive slot, on a class loaded from real JDK bytes. _ALL=1 is required
#     or the java.util.Map suppression hides the dominant family. Measured: 13
#     classes, 24 slots, from three small probes -- and identical under
#     --real-jdk and --jdk-only, so it is a Compatible-mode defect too.
#
# Usage:
#   JAVA_HOME=/path/to/real/jdk \
#   CV=/path/to/target/release/cratonvm \
#   PROBES=/path/to/compiled/probe/classes \
#   [OUT=/tmp/jdk-only-measure] \
#   scripts/jdk-only-measure-refusals-and-overlays.sh
#
# PROBES must hold compiled JdkOnlyIcHotProbe, JdkOnlyCensusLoadProbe and
# JdkOnlyBreadthProbe from probes/.
set -u

: "${JAVA_HOME:?set JAVA_HOME to a REAL JDK image, not a synthetic one}"
: "${CV:?set CV to the cratonvm binary under test}"
: "${PROBES:?set PROBES to the directory holding the compiled probe classes}"
OUT="${OUT:-/tmp/jdk-only-measure}"
mkdir -p "$OUT"
LOG="$OUT/measure.log"
: > "$LOG"

cd "$PROBES" || exit 1
ALL_PROBES="JdkOnlyIcHotProbe JdkOnlyCensusLoadProbe JdkOnlyBreadthProbe"

echo "########## A. inline-cache / direct-bind / fastpath refusals" >> "$LOG"
for P in $ALL_PROBES; do
  timeout 300 "$CV" --jdk-only --java-home "$JAVA_HOME" \
      --jdk-only-report "$OUT/rep-$P.json" -cp . "$P" > "$OUT/m-$P.txt" 2>&1
  rc=$?
  echo "--- $P exit=$rc ---" >> "$LOG"
  # A non-zero exit means the run did not finish, so its counters describe a
  # partial run. Say so rather than reporting them as a measurement.
  if [ $rc -ne 0 ]; then
    echo "  RUN DID NOT COMPLETE - counters below are partial" >> "$LOG"
  fi
  grep -E "^(ICHOT|CENSUSLOAD|PROBE2|SECTION-FAILED)" "$OUT/m-$P.txt" >> "$LOG"
  python3 - "$OUT/rep-$P.json" >> "$LOG" 2>&1 <<'PY'
import json, sys
try:
    d = json.load(open(sys.argv[1]))
except Exception as e:
    print("  NO REPORT:", e)
    sys.exit()
def find(obj, key, path="d"):
    if isinstance(obj, dict):
        for k, v in obj.items():
            if k == key:
                print(f"  {key} = {v}   (at {path})")
            find(v, key, path + "." + k)
for k in ("jit_direct_native_binds", "jit_inline_cache_natives",
          "jit_fastpath_admissions", "interpreter_bytecode_preferred"):
    find(d, k)
PY
done

echo "" >> "$LOG"
echo "########## B. overlay-corruption census (both modes)" >> "$LOG"
for MODE in --real-jdk --jdk-only; do
  for P in $ALL_PROBES; do
    CRATONVM_DBG_OVERLAY=1 CRATONVM_DBG_OVERLAY_ALL=1 \
      timeout 300 "$CV" "$MODE" --java-home "$JAVA_HOME" -cp . "$P" \
      > "$OUT/ov-$MODE-$P.txt" 2>&1
    rc=$?
    n=$(grep -ci overlay "$OUT/ov-$MODE-$P.txt")
    echo "--- $MODE $P exit=$rc lines=$n ---" >> "$LOG"
  done
done

echo "" >> "$LOG"
echo "=== distinct (class, slot, value kind, real descriptor) sites ===" >> "$LOG"
cat "$OUT"/ov-*.txt 2>/dev/null \
  | grep "destructive native set_field" \
  | sed -E 's/value=(Int|Long|Float|Double|Object)\([^)]*\)/value=\1/' \
  | sed -E 's/^.*class=/class=/' \
  | sort | uniq -c | sort -rn >> "$LOG"

echo "MEASURECOMPLETE" >> "$LOG"
cat "$LOG"
