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
#     primitive slot, on a class loaded from real JDK bytes. The overlay-all
#     token is required or the java.util.Map suppression hides the dominant
#     family. Measured: 13 classes, 24 slots, from three small probes -- and
#     identical under --real-jdk and --jdk-only, so it is a Compatible-mode
#     defect too. VarHandle was fixed the same day, taking it to 11 classes /
#     21 slots.
#
# Usage:
#   JAVA_HOME=/path/to/real/jdk \
#   CV=/path/to/target/release/cratonvm \
#   PROBES=/path/to/compiled/probe/classes \
#   [OUT=/tmp/jdk-only-measure] \
#   scripts/jdk-only-measure-refusals-and-overlays.sh
#
# PROBES must hold compiled JdkOnlyIcHotProbe, JdkOnlyCensusLoadProbe and
# JdkOnlyBreadthProbe from apps/probes/.
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
          "jit_fastpath_admissions", "interpreter_bytecode_preferred",
          # Not a refusal: the §1.4 shadows step 1 OBSERVED and let run. It
          # rides in the same block because a list of what strict mode stopped,
          # with no line for what it saw and did not stop, reads as zero.
          "interpreter_shadow_unenforced"):
    find(d, k)
PY
done

echo "" >> "$LOG"
echo "########## B. overlay-corruption census (both modes)" >> "$LOG"
PARTIAL_RUNS=0
for MODE in --real-jdk --jdk-only; do
  for P in $ALL_PROBES; do
    # Grouped spelling. The per-flag CRATONVM_DBG_OVERLAY* variables still work
    # but the VM prints a deprecation line for each run that uses them.
    CRATONVM_DBG=overlay,overlay-all \
      timeout 300 "$CV" "$MODE" --java-home "$JAVA_HOME" -cp . "$P" \
      > "$OUT/ov-$MODE-$P.txt" 2>&1
    rc=$?
    n=$(grep -ci overlay "$OUT/ov-$MODE-$P.txt")
    # A run that did not reach its own completion line contributes a PARTIAL
    # census that looks exactly like a small clean one -- the counts below are
    # then silently short. `JdkOnlyCensusLoadProbe` deadlocked in its `net`
    # section roughly 1 run in 20 until 2026-08-05 (a registry read guard held
    # across the listener poll), and during that period a truncated run was
    # briefly read as a behavioural difference between two binaries. Check the
    # terminator, not just the exit code: a probe can also fail a section and
    # still exit 0.
    case "$P" in
      JdkOnlyCensusLoadProbe) want="CENSUSLOAD sections=" ;;
      JdkOnlyBreadthProbe)    want="PROBE2 sections=" ;;
      JdkOnlyIcHotProbe)      want="ICHOT done" ;;
      *)                      want="" ;;
    esac
    complete=yes
    if [ -n "$want" ] && ! grep -q "$want" "$OUT/ov-$MODE-$P.txt"; then
      complete=no
      PARTIAL_RUNS=$((PARTIAL_RUNS + 1))
    fi
    echo "--- $MODE $P exit=$rc complete=$complete lines=$n ---" >> "$LOG"
  done
done

if [ "$PARTIAL_RUNS" -gt 0 ]; then
  echo "" >> "$LOG"
  echo "!!! $PARTIAL_RUNS of the census runs above did NOT complete. Every count" >> "$LOG"
  echo "!!! below is a FLOOR of a floor -- do not A/B against it." >> "$LOG"
fi

echo "" >> "$LOG"
echo "=== distinct (op, class, slot, value kind, real descriptor) sites ===" >> "$LOG"
# The hunter's line reads `suspect native set_field [reasons]:` / `... get_field
# [reasons]:` since L4 (2026-08-05) — it used to read `destructive native
# set_field:` and cover writes only. Both spellings are matched so this script
# can score a PRE-L4 binary and a post-L4 one in the same A/B, which is the
# whole point of running it twice.
cat "$OUT"/ov-*.txt 2>/dev/null \
  | grep -E "(destructive|suspect) native (set|get)_field" \
  | sed -E 's/value=(Int|Long|Float|Double|Object)\([^)]*\)/value=\1/' \
  | sed -E 's/ model=[^ ]* real=[^ ]* verdict=[A-Za-z]+//' \
  | sed -E 's/^.*(set_field|get_field)[^:]*:.*class=/\1 class=/' \
  | sort | uniq -c | sort -rn >> "$LOG"

echo "" >> "$LOG"
echo "=== shadow-layout diff: classes whose model disagrees with the image ===" >> "$LOG"
# L4 step 3. One line per class, emitted at define time, independent of whether
# any native ever touches the class — so this half of the census does not depend
# on the probe reaching the code.
cat "$OUT"/ov-*.txt 2>/dev/null \
  | grep "^\[OVERLAY-LAYOUT\] " \
  | grep -v "^\[OVERLAY-LAYOUT\]   " \
  | sed -E 's/^\[OVERLAY-LAYOUT\] //' \
  | sort -u >> "$LOG"

echo "" >> "$LOG"
echo "=== shadow-layout diff: disagreeing slots ===" >> "$LOG"
cat "$OUT"/ov-*.txt 2>/dev/null \
  | grep -E "^\[OVERLAY-LAYOUT\]   slot .* (TYPE|NAME) " \
  | sed -E 's/^\[OVERLAY-LAYOUT\]   //' \
  | sort | uniq -c | sort -rn >> "$LOG"

if [ "$PARTIAL_RUNS" -gt 0 ]; then
  echo "MEASUREPARTIAL runs_incomplete=$PARTIAL_RUNS" >> "$LOG"
else
  echo "MEASURECOMPLETE" >> "$LOG"
fi
cat "$LOG"
