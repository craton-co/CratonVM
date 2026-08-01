#!/usr/bin/env bash
# Which liveness ARM condemns a LIVE overlay-backed collection?
#
# `process_references_after_gc`'s overlay prune destroys state on a `false`
# from `is_addr_live`, whose two arms (old-gen allocated / young survivor) are
# indistinguishable in the verdict. `CRATONVM_DBG_OVERLAY_PRUNE=1` decomposes
# each condemnation via `VmHeap::liveness_arms`, so the arm is a measurement
# rather than an inference.
#
# ROverlaySystemGcStress is the seconds-long reproducer: overlay-backed
# collections kept live across explicit `System.gc()` rounds so the old ones
# get promoted.
#
#   CV=<cratonvm.exe>   binary under test (required)
#   TAG=<name>          output tag
#   MODE=jit|nojit      default jit (the arm the doc measured)
set -uo pipefail
export MSYS2_ARG_CONV_EXCL='*'
export MSYS_NO_PATHCONV=1

HERE="$(cd "$(dirname "$0")/.." && pwd)"
CV="${CV:?set CV to the cratonvm.exe under test}"
TAG="${TAG:-arms}"
MODE="${MODE:-jit}"
BUILD="${BUILD:-$HERE/regression-suite/build}"
OUT="${OUT:-$HERE/regression-suite/runs/overlay-prune-$TAG.log}"

detect_jdk() {
  local c
  for c in "C:/Program Files/Eclipse Adoptium"/jdk-25* \
           "C:/Program Files/Java"/jdk-25* \
           "C:/Program Files/Eclipse Adoptium"/jdk-2* \
           "C:/Program Files/Java"/jdk-2*; do
    [ -x "$c/bin/java.exe" ] && { printf '%s' "$c"; return 0; }
  done
  return 1
}
JDK="${JDK:-$(detect_jdk)}"
[ -n "$JDK" ] || { echo "no JDK found" >&2; exit 2; }

VMFLAGS=(--java-home "$JDK" -cp "$BUILD")
[ "$MODE" = nojit ] && VMFLAGS+=(--nojit)

mkdir -p "$(dirname "$OUT")"
echo "=== $TAG MODE=$MODE CV=$CV JDK=$JDK ===" | tee "$OUT"
CRATONVM_DBG_OVERLAY_PRUNE=1 "$CV" "${VMFLAGS[@]}" ROverlaySystemGcStress >>"$OUT" 2>&1
rc=$?
echo "=== rc=$rc ===" | tee -a "$OUT"

# The decomposition. A CONDEMNED line with old_gen_allocated=false AND
# young_survivor=false but region=young is the wrong-semispace arm; region=
# off-heap is "the predicate cannot evaluate this address at all". Either way
# the prune is destroying state on absence-of-evidence.
echo "--- condemnations by (region, arms) ---" | tee -a "$OUT"
grep -o 'region=[a-z-]* .*old_gen_allocated=[a-z]* young_survivor=[a-z]*' "$OUT" \
  | sed 's/ in_pointer_map=[a-z]*//' | sort | uniq -c | sort -rn | tee -a "$OUT"
echo "--- totals ---" | tee -a "$OUT"
{ grep -c 'CONDEMNED' "$OUT" | sed 's/^/condemned=/'
  grep -c 'dead_keys' "$OUT" | sed 's/^/dead_keys_lines=/'
  grep -m3 -E 'AssertionError|ClassCastException|TMDIAG' "$OUT"; } | tee -a "$OUT"
exit $rc
