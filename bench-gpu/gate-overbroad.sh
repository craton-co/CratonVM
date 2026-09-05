#!/usr/bin/env bash
# How much of a program does `--gpu` deny to the JIT, and why?
#
# `offload_jit_gate` blocks a method for one of two reasons, and both
# were far wider than what they protected until 2026-09-04:
#
#   writes-primitive-array   any `*astore`, because the compiled tiers
#                            could not evict the residency cache
#   calls-eligible-kernel    any `invokestatic` the ANALYZER likes,
#                            including `java/lang/Math.min(II)I`
#
# Both narrowings ship with a kill switch, so this is a three-arm run of
# ONE binary -- no cross-binary comparison, and each switch isolates the
# narrowing it belongs to:
#
#   default   barrier armed, dispatchability checked
#   arm B     CRATONVM_JIT_GPU_ARRAY_BARRIER=0     -> array writers blocked
#   arm C     CRATONVM_GPU_JIT_GATE_DISPATCHABLE=0 -> Math.min callers blocked
#
# The assertion is on the CENSUS, not on wall clock: at this fixture's
# size a desktop timing would be noise, and "which methods were denied
# compilation" is the fact under test. `GpuJitWriterStale` covers the
# correctness half -- that admitting the writers does not stale the
# cache.
#
# Usage:
#   CV=path/to/cratonvm.exe JDK=path/to/jdk bash bench-gpu/gate-overbroad.sh [rounds]
#
# Exit: 0 if the default arm blocks strictly fewer methods than both
# control arms, and blocks neither shape by name.
set -u
export MSYS2_ARG_CONV_EXCL='*' MSYS_NO_PATHCONV=1

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"

CV="${CV:-$ROOT/target-gpu/release/cratonvm.exe}"
JDK="${JDK:-C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot}"
TG="${TG:-$ROOT/test_classes/gpu}"
ROUNDS="${1:-200000}"

if [ ! -f "$TG/GpuGateOverBroad.class" ]; then
  echo "compiling fixture into $TG"
  (cd "$ROOT" && "$JDK/bin/javac" -d "$TG" "test_classes/gpu/GpuGateOverBroad.java") || exit 1
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

echo "=== gpu jit gate breadth (rounds=$ROUNDS) ==="
echo "CV=$CV"
echo

run_arm() {
  # $1 = label, rest = env assignments
  local label="$1"; shift
  env "$@" "$CV" --java-home "$JDK" -cp "$TG" --gpu \
      GpuGateOverBroad "$ROUNDS" > "$TMP/$label.out" 2> "$TMP/$label.log"
  grep 'blocked_from_jit' "$TMP/$label.log" | head -1
}

echo "--- default (both narrowings on) ---"
run_arm default CRATONVM_DUMMY=1
grep 'gpu jit gate:' "$TMP/default.log" | sed 's/^/  /'
echo
echo "--- arm B: CRATONVM_JIT_GPU_ARRAY_BARRIER=0 ---"
run_arm armB CRATONVM_JIT_GPU_ARRAY_BARRIER=0
echo
echo "--- arm C: CRATONVM_GPU_JIT_GATE_DISPATCHABLE=0 ---"
run_arm armC CRATONVM_GPU_JIT_GATE_DISPATCHABLE=0
echo

blocked() {
  grep -o 'blocked_from_jit=[0-9]*' "$TMP/$1.log" | head -1 | cut -d= -f2
}
D=$(blocked default); B=$(blocked armB); C=$(blocked armC)
echo "blocked_from_jit:  default=$D  armB(no barrier)=$B  armC(no dispatch check)=$C"

FAILS=0

# The three arms must all have produced a census. A missing one means
# the run never reached the gate, which would make every comparison
# below vacuous.
for a in default armB armC; do
  if [ -z "$(blocked $a)" ]; then
    echo "FAIL: the '$a' arm printed no gate census -- the gate never ran"
    FAILS=$((FAILS + 1))
  fi
  if ! grep -q 'gate_ints=' "$TMP/$a.out"; then
    echo "FAIL: the '$a' arm did not run to completion"
    FAILS=$((FAILS + 1))
  fi
done
[ "$FAILS" -gt 0 ] && exit 1

# Every arm must agree on the answer: a narrowing that changes what the
# program computes is not a narrowing.
if ! diff -q "$TMP/default.out" "$TMP/armB.out" > /dev/null \
   || ! diff -q "$TMP/default.out" "$TMP/armC.out" > /dev/null; then
  echo "FAIL: the arms disagree on the program's OUTPUT"
  diff "$TMP/default.out" "$TMP/armB.out" | sed 's/^/       /'
  diff "$TMP/default.out" "$TMP/armC.out" | sed 's/^/       /'
  FAILS=$((FAILS + 1))
fi

# Each switch must actually engage. If a control arm blocks no more than
# the default, the switch is dead and this script is measuring nothing.
if [ "$B" -le "$D" ]; then
  echo "FAIL: CRATONVM_JIT_GPU_ARRAY_BARRIER=0 blocked $B, default blocked $D --"
  echo "      the switch did not restore the old array-writer refusal"
  FAILS=$((FAILS + 1))
else
  echo "ok: the barrier releases $((B - D)) array writer(s)"
fi
if [ "$C" -le "$D" ]; then
  echo "FAIL: CRATONVM_GPU_JIT_GATE_DISPATCHABLE=0 blocked $C, default blocked $D --"
  echo "      the switch did not restore the old eligible-target refusal"
  FAILS=$((FAILS + 1))
else
  echo "ok: the dispatchability check releases $((C - D)) caller(s)"
fi

# ...and the default must not block the two shapes by name.
for m in 'GpuGateOverBroad\$Vec3.set' 'GpuGateOverBroad.clamp'; do
  if grep -q "blocked .*$m" "$TMP/default.log"; then
    echo "FAIL: the default arm still blocks $m"
    grep "blocked .*$m" "$TMP/default.log" | sed 's/^/       /'
    FAILS=$((FAILS + 1))
  fi
done

echo
if [ "$FAILS" = "0" ]; then
  echo "PASS"
  exit 0
fi
echo "FAIL ($FAILS)"
exit 1
