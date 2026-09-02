#!/usr/bin/env bash
# Differential oracle for the opcodes jit-cuda lowers BY HAND.
#
# Runs every kernel of test_classes/gpu/GpuArithDifferential.java three
# ways and compares them element by element:
#
#   HotSpot            the reference
#   cratonvm --nojit   the CONTROL. Without it a difference cannot be
#                      attributed: "GPU disagrees with HotSpot" is also
#                      what a host-side defect looks like, and on
#                      2026-09-02 a String.equals miscompile in the JIT
#                      made this harness blame the emitter for the host
#                      picking the wrong printf branch. The control is
#                      what separated them.
#   cratonvm --gpu     the arm under test
#
# Inputs are built from raw bit patterns — signalling and quiet NaNs with
# distinct payloads, both zeros, both infinities, subnormals, the
# saturation boundaries of every float->int conversion, MIN_VALUE/-1 for
# the division guards, and shift counts past the operand width. Results
# are compared as RAW BITS, because `-0.0 == 0.0` and `NaN != NaN` would
# hide exactly the cases these inputs exist to probe.
#
# Usage:
#   CV=path/to/cratonvm.exe JDK=path/to/jdk bash bench-gpu/arith-differential.sh [n]
#
# Windows note: CV/JDK/CP must be Windows-style (C:/...); an MSYS /c/...
# path fails with "path does not exist".
#
# Exit: 0 if the GPU arm matches the control on every kernel, 1 otherwise.
set -u
export MSYS2_ARG_CONV_EXCL='*' MSYS_NO_PATHCONV=1

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"

CV="${CV:-$ROOT/target-gpu/release/cratonvm.exe}"
JDK="${JDK:-C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot}"
TG="${TG:-$(cd "$ROOT/test_classes/gpu" && pwd -W 2>/dev/null || echo "$ROOT/test_classes/gpu")}"
N="${1:-4096}"

if [ ! -f "$TG/GpuArithProbe.class" ]; then
  echo "compiling fixtures into $TG"
  "$JDK/bin/javac" -d "$TG" "$ROOT/test_classes/gpu/GpuArithDifferential.java" \
                            "$ROOT/test_classes/gpu/GpuArithProbe.java" || exit 1
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
FAILS=0

# 0 fadd  1 fmul  2 fdiv  3 fneg  4 d2i  5 f2i  6 f2l  7 d2l
NAMES=(fadd fmul fdiv fneg d2i f2i f2l d2l)

echo "=== jit-cuda arithmetic differential (n=$N) ==="
echo "CV=$CV"

for k in 0 1 2 3 4 5 6 7; do
  name="${NAMES[$k]}"
  # The trailing `1` collapses NaN payloads. They are unspecified by
  # both JLS 4.2.3 and the PTX ISA, and the device canonicalises where
  # HotSpot propagates, so comparing them raw reports a difference that
  # is real, permanent, and not a defect — which would make this
  # unusable as a gate. Drop the `1` to see the payloads when
  # investigating one.
  "$JDK/bin/java" -cp "$TG" GpuArithProbe "$k" "$N" 1 2>/dev/null \
      | grep -E '^[0-9]+ ' | tr -d '\r' > "$TMP/hs"
  "$CV" --java-home "$JDK" -cp "$TG" --nojit GpuArithProbe "$k" "$N" 1 2>/dev/null \
      | grep -E '^[0-9]+ ' | tr -d '\r' > "$TMP/cpu"
  "$CV" --java-home "$JDK" -cp "$TG" --gpu GpuArithProbe "$k" "$N" 1 2>/dev/null \
      | grep -E '^[0-9]+ ' | tr -d '\r' > "$TMP/gpu"

  if [ ! -s "$TMP/hs" ] || [ ! -s "$TMP/cpu" ] || [ ! -s "$TMP/gpu" ]; then
    echo "FAIL $name: one of the three arms produced no output"
    FAILS=$((FAILS + 1))
    continue
  fi

  host=$(diff "$TMP/hs" "$TMP/cpu" | grep -c '^<')
  dev=$(diff "$TMP/cpu" "$TMP/gpu" | grep -c '^<')

  if [ "$host" != "0" ]; then
    # Not this script's finding to report as a GPU defect — say so.
    echo "SKIP $name: the CPU control already differs from HotSpot in \
$host element(s); fix that before reading the device arm"
    FAILS=$((FAILS + 1))
    continue
  fi

  if [ "$dev" = "0" ]; then
    echo "PASS $name"
  else
    echo "FAIL $name: $dev of $N element(s) differ from the CPU control"
    diff "$TMP/cpu" "$TMP/gpu" | grep '^[<>]' | head -6 | sed 's/^/       /'
    FAILS=$((FAILS + 1))
  fi
done

# The aggregate too: it covers the kernels the per-element probe does not
# carry (shifts, integer division, the narrowing conversions).
"$JDK/bin/java" -cp "$TG" GpuArithDifferential "$N" 2>/dev/null \
    | grep '=' | tr -d '\r' > "$TMP/agg_hs"
"$CV" --java-home "$JDK" -cp "$TG" --nojit GpuArithDifferential "$N" 2>/dev/null \
    | grep '=' | tr -d '\r' > "$TMP/agg_cpu"
"$CV" --java-home "$JDK" -cp "$TG" --gpu GpuArithDifferential "$N" 2>/dev/null \
    | grep '=' | tr -d '\r' > "$TMP/agg_gpu"

agg=$(diff "$TMP/agg_cpu" "$TMP/agg_gpu" | grep -c '^<')
if [ "$agg" = "0" ]; then
  echo "PASS aggregate (27 kernels)"
else
  echo "FAIL aggregate: $agg checksum(s) differ from the CPU control"
  diff "$TMP/agg_cpu" "$TMP/agg_gpu" | grep '^[<>]' | head -12 | sed 's/^/       /'
  FAILS=$((FAILS + 1))
fi

echo "=== summary ==="
if [ "$FAILS" = "0" ]; then
  echo "ALL DIFFERENTIAL CHECKS PASSED"
  exit 0
fi
echo "$FAILS DIFFERENTIAL CHECK(S) FAILED"
exit 1
