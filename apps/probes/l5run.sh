#!/usr/bin/env bash
# l5run.sh <Probe> [more...] — the differential runner for lane L5
# (`java.util.concurrent`, `Thread`, `Unsafe`).
#
#   oracle    HotSpot   ($JDK, default /data/jdkimages/jdk25-linux/jdk-25.0.4+7)
#   strict    cratonvm --jdk-only
#   armed     cratonvm --jdk-only with $SCOPE on the shadow dial (optional)
#
# Modelled on `l4run.sh`, with one addition this lane needs.
#
# **`--add-exports` is applied to `java` AND to `javac`, on BOTH VMs.** Two of
# this lane's probes (`L5CasRace`, `L5SubwordAtomics`) reach
# `jdk.internal.misc.Unsafe`, which is the only way anyone reaches it. The
# whole-tree battery (`scripts/jdk-only-phase2-battery.sh`) compiles without the
# flag, so it reports those two JAVAC-FAILED and EXCLUDES them from its counts
# — correctly, and visibly, which is why this script exists rather than a
# widening of the battery. A harness that compiled them without the flag and
# diffed the runs anyway would compare an empty file with an empty file and
# report zero differences.
#
# Check the `rows` line, printed before the diff, for the same reason `l4run.sh`
# does: a run that died partway produces a short file whose missing tail `diff`
# reports as ordinary `<` lines.
set +e
export MSYS2_ARG_CONV_EXCL='*'
export MSYS_NO_PATHCONV=1

W="$(git -C "$(dirname "${BASH_SOURCE[0]}")" rev-parse --show-toplevel 2>/dev/null)"
if [ -z "$W" ] || [ ! -d "$W/apps/probes" ]; then
  echo "l5run.sh: cannot resolve the repository root — run it from inside its worktree." >&2
  exit 3
fi
CV="${CV:-$W/target/release/cratonvm}"
[ -x "$CV" ] || CV="$W/target/release/cratonvm.exe"
JDK="${JDK:-${JAVA_HOME:-/data/jdkimages/jdk25-linux/jdk-25.0.4+7}}"
OUT="${OUT:-${TMPDIR:-/tmp}/l5out}"
SCOPE="${SCOPE:-}"
mkdir -p "$OUT" "$W/apps/probes/out"
cd "$W" || exit 1
export CRATONVM_DISABLE_DEFAULT_WATCHDOG=1

XPORTS="--add-exports java.base/jdk.internal.misc=ALL-UNNAMED"

for C in "$@"; do
  # Compile with the exports every time: a stale class from a run without them
  # is the false green this script exists to prevent.
  if ! "$JDK/bin/javac" $XPORTS -d "$W/apps/probes/out" "$W/apps/probes/$C.java" 2>"$OUT/$C.javac"; then
    echo "=== $C   JAVAC-FAILED"
    head -5 "$OUT/$C.javac"
    continue
  fi
  timeout 900 "$JDK/bin/java" $XPORTS -cp "$W/apps/probes/out" "$C" > "$OUT/$C.oracle" 2>/dev/null
  o=$?
  timeout 900 "$CV" --java-home "$JDK" --jdk-only $XPORTS -cp "$W/apps/probes/out" "$C" > "$OUT/$C.strict" 2>/dev/null
  s=$?
  a=-1
  if [ -n "$SCOPE" ]; then
    timeout 900 env CRATONVM_ENFORCE_NATIVE_SHADOW="$SCOPE" \
      "$CV" --java-home "$JDK" --jdk-only $XPORTS -cp "$W/apps/probes/out" "$C" \
      > "$OUT/$C.armed" 2>/dev/null
    a=$?
  fi
  echo "=== $C   rc oracle=$o strict=$s armed=$a"
  echo "    lines  oracle=$(wc -l < "$OUT/$C.oracle")  strict=$(wc -l < "$OUT/$C.strict")$([ -n "$SCOPE" ] && echo "  armed=$(wc -l < "$OUT/$C.armed")")"
  echo "    rows   oracle $(grep -a '^rows ' "$OUT/$C.oracle") | strict $(grep -a '^rows ' "$OUT/$C.strict")$([ -n "$SCOPE" ] && echo " | armed $(grep -a '^rows ' "$OUT/$C.armed")")"
  ds=$(diff "$OUT/$C.oracle" "$OUT/$C.strict" | grep -ac '^[<>]')
  diff "$OUT/$C.oracle" "$OUT/$C.strict" > "$OUT/$C.strict.diff"
  if [ -n "$SCOPE" ]; then
    da=$(diff "$OUT/$C.oracle" "$OUT/$C.armed" | grep -ac '^[<>]')
    diff "$OUT/$C.oracle" "$OUT/$C.armed" > "$OUT/$C.armed.diff"
    echo "    DIFF   d(hs,strict)=$ds  d(hs,armed)=$da  delta=$((da - ds))"
  else
    echo "    DIFF   d(hs,strict)=$ds"
  fi
done
echo "L5RUN-DONE"
