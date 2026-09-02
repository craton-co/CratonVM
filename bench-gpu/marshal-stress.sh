#!/usr/bin/env bash
# Adversarial cover for `vm/src/runtime/gpu_marshal.rs`.
#
# The marshaller has SIX element types and TWO transfer paths for each:
# a zero-copy one that hands the JVM heap arena straight to the DMA, and
# a staged one through `host_view_*` + `write_back_*`. `ci-gate.sh`
# exercises int[] and float[] only.
#
# FOUR arms:
#
#   HotSpot            the oracle
#   cratonvm --nojit   the CONTROL. "the GPU disagrees with HotSpot" is
#                      also what a host-side defect looks like.
#   cratonvm --gpu     zero-copy transfers
#   cratonvm --gpu     with CRATONVM_GPU_NO_ZEROCOPY=1: staged transfers.
#                      The two paths must agree byte for byte; if they do
#                      not, one of them is wrong.
#
# ── the engagement census, and why it is not optional ────────────────
#
# On 2026-09-02 all three value arms of this differential came back
# byte-identical while `short[]` and `byte[]` were never offloaded at
# all: the analyzer admitted those kernels and the lowering emitted PTX,
# but `dispatch_method_from_native`'s `match element_type` had no Short
# or Byte arm, so each dispatch died at marshalling with "unsupported
# array element type" and silently re-ran on the interpreter. A
# value-only differential CANNOT see that -- it was comparing the
# interpreter with itself and reporting a pass.
#
# So this script counts, per kernel, the H2D transfers the offload path
# logged, and FAILS if any of the six never dispatched. A correctness
# arm without an engagement census is not a test of the GPU.
#
# Usage:
#   CV=path/to/cratonvm.exe JDK=path/to/jdk bash bench-gpu/marshal-stress.sh [n]
#
# Windows note: CV/JDK/TG must be Windows-style (C:/...); an MSYS /c/...
# path fails with "path does not exist".
#
# Exit: 0 if every arm agrees AND all six kernels actually offloaded.
set -u
export MSYS2_ARG_CONV_EXCL='*' MSYS_NO_PATHCONV=1

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"

CV="${CV:-$ROOT/target-gpu/release/cratonvm.exe}"
JDK="${JDK:-C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot}"
TG="${TG:-$ROOT/test_classes/gpu}"
N="${1:-131072}"

if [ ! -f "$TG/GpuMarshalStress.class" ]; then
  echo "compiling fixture into $TG"
  "$JDK/bin/javac" -d "$TG" "$ROOT/test_classes/gpu/GpuMarshalStress.java" || exit 1
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

echo "=== gpu marshalling stress (n=$N) ==="
echo "CV=$CV"

"$JDK/bin/java" -cp "$TG" GpuMarshalStress 0 "$N" 2>/dev/null \
    | grep '=' | tr -d '\r' > "$TMP/hs"
"$CV" --java-home "$JDK" -cp "$TG" --nojit GpuMarshalStress 0 "$N" 2>/dev/null \
    | grep '=' | tr -d '\r' > "$TMP/cpu"
RUST_LOG="cratonvm_vm::runtime::offload=info" CRATONVM_GPU_TRACE_BYTES=1 \
    "$CV" --java-home "$JDK" -cp "$TG" --gpu GpuMarshalStress 0 "$N" \
    > "$TMP/gpu.out" 2> "$TMP/gpu.log"
grep '=' "$TMP/gpu.out" | tr -d '\r' > "$TMP/gpu"
CRATONVM_GPU_NO_ZEROCOPY=1 \
    "$CV" --java-home "$JDK" -cp "$TG" --gpu GpuMarshalStress 0 "$N" 2>/dev/null \
    | grep '=' | tr -d '\r' > "$TMP/staged"

for f in hs cpu gpu staged; do
  if [ ! -s "$TMP/$f" ]; then
    echo "FAIL: the '$f' arm produced no output"
    exit 1
  fi
done

FAILS=0

# ── 1. the control must match the oracle before anything else is read ──
host=$(diff "$TMP/hs" "$TMP/cpu" | grep -c '^<')
if [ "$host" != "0" ]; then
  echo "FAIL: the CPU control already differs from HotSpot in $host line(s)."
  echo "      Fix that before reading the device arms -- a host-side defect"
  echo "      looks exactly like a GPU one from here."
  diff "$TMP/hs" "$TMP/cpu" | grep '^[<>]' | sed 's/^/       /'
  exit 1
fi
echo "control matches HotSpot on all $(wc -l < "$TMP/cpu") scenarios"

# ── 2. the engagement census ───────────────────────────────────────────
echo
echo "--- engagement census (H2D dispatches per kernel) ---"
for k in scaleI scaleJ scaleF scaleD scaleS scaleB; do
  c=$(grep -c "H2D=.*(GpuMarshalStress\.$k" "$TMP/gpu.log" 2>/dev/null || true)
  if [ "${c:-0}" -gt 0 ]; then
    echo "  $k: $c transfer(s)"
  else
    echo "  $k: NEVER DISPATCHED -- this kernel's arm is vacuous"
    if grep -q "unsupported array element type" "$TMP/gpu.log"; then
      echo "      (marshaller refused it:"
      grep -o "unsupported array element type: [A-Za-z]*" "$TMP/gpu.log" \
        | sort -u | sed 's/^/       /'
      echo "      )"
    fi
    FAILS=$((FAILS + 1))
  fi
done

# ── 3. both device arms against the control ────────────────────────────
echo
echo "--- values ---"
while IFS= read -r line; do
  key="${line%%=*}"
  want="${line#*=}"
  for arm in gpu staged; do
    got=$(grep "^$key=" "$TMP/$arm" | head -1)
    got="${got#*=}"
    if [ "$got" != "$want" ]; then
      echo "FAIL $key ($arm): control=$want got=$got"
      FAILS=$((FAILS + 1))
    fi
  done
done < <(grep '=' "$TMP/cpu")
if [ "$FAILS" = "0" ]; then
  echo "  all scenarios match the control on both the zero-copy and staged paths"
fi

echo
echo "=== summary ==="
if [ "$FAILS" = "0" ]; then
  echo "ALL MARSHALLING SCENARIOS PASSED (and all six kernels really offloaded)"
  exit 0
fi
echo "$FAILS CHECK(S) FAILED"
exit 1
