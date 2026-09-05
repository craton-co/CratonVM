#!/usr/bin/env bash
# Adversarial cover for the VM-side offload runtime.
#
# `vm/src/runtime/offload.rs` has 22 unit tests and every one of them is a
# no-device or error path — none of it can run without a device, so none
# of them dispatches a kernel. The happy path (marshalling, the
# input-residency cache, the read-only-input write suppression, the
# chunked writeback, the bounds deopt, the write-back into the Java heap)
# had `bench-gpu/ci-gate.sh`'s five end-to-end checks and nothing else.
#
# This runs the scenarios those five do not reach. THREE arms, and the
# middle one is not optional:
#
#   HotSpot            the oracle
#   cratonvm           the COMPILED CPU arm. Added 2026-09-04, after it
#                      turned out no arm here had ever run this fixture
#                      through the JIT: HotSpot is the oracle, `--nojit`
#                      is interpreted by construction, and under `--gpu`
#                      `offload_jit_gate` REFUSES to compile every
#                      scenario that writes an array or calls a kernel --
#                      which is all of them. An OSR miscompilation of
#                      `cacheCoherence` sat behind that hole; see
#                      docs/known-issues/jit/osr-miscompiles-cachecoherence-20260904.md.
#   cratonvm --nojit   the CONTROL. "The GPU disagrees with HotSpot" is
#                      also what a host-side defect looks like; without
#                      the control a difference cannot be attributed.
#   cratonvm --gpu     the arm under test
#
# The script refuses to report a device difference while the control
# itself disagrees with HotSpot.
#
# Usage:
#   CV=path/to/cratonvm.exe JDK=path/to/jdk bash bench-gpu/runtime-stress.sh [n]
#
# Windows note: CV/JDK/TG must be Windows-style (C:/...); an MSYS /c/...
# path fails with "path does not exist".
#
# Exit: 0 if the GPU arm matches the control on every scenario.
set -u
export MSYS2_ARG_CONV_EXCL='*' MSYS_NO_PATHCONV=1

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"

CV="${CV:-$ROOT/target-gpu/release/cratonvm.exe}"
JDK="${JDK:-C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot}"
TG="${TG:-$ROOT/test_classes/gpu}"
N="${1:-65536}"

if [ ! -f "$TG/GpuRuntimeStress.class" ]; then
  echo "compiling fixture into $TG"
  # Relative source path from $ROOT: javac is a Windows binary and
  # cannot open the MSYS-style "/c/..." that $ROOT expands to.
  (cd "$ROOT" && "$JDK/bin/javac" -d "$TG" "test_classes/gpu/GpuRuntimeStress.java") || exit 1
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

echo "=== vm offload runtime stress (n=$N) ==="
echo "CV=$CV"

"$JDK/bin/java" -cp "$TG" GpuRuntimeStress 0 "$N" 2>/dev/null \
    | grep '=' | tr -d '\r' > "$TMP/hs"
"$CV" --java-home "$JDK" -cp "$TG" --nojit GpuRuntimeStress 0 "$N" 2>/dev/null \
    | grep '=' | tr -d '\r' > "$TMP/cpu"
"$CV" --java-home "$JDK" -cp "$TG" GpuRuntimeStress 0 "$N" 2>/dev/null \
    | grep '=' | tr -d '\r' > "$TMP/jit"
"$CV" --java-home "$JDK" -cp "$TG" --gpu GpuRuntimeStress 0 "$N" 2>/dev/null \
    | grep '=' | tr -d '\r' > "$TMP/gpu"

for f in hs cpu gpu; do
  if [ ! -s "$TMP/$f" ]; then
    echo "FAIL: the '$f' arm produced no output"
    exit 1
  fi
done

host=$(diff "$TMP/hs" "$TMP/cpu" | grep -c '^<')
if [ "$host" != "0" ]; then
  echo "FAIL: the CPU control already differs from HotSpot in $host line(s)."
  echo "      Fix that before reading the device arm — a host-side defect"
  echo "      looks exactly like a GPU one from here."
  diff "$TMP/hs" "$TMP/cpu" | grep '^[<>]' | sed 's/^/       /'
  exit 1
fi

FAILS=0

# The compiled CPU arm, against the same control. No `--gpu`, so nothing
# here involves the device: a difference is a JIT defect, and one that
# every other arm in this file is structurally unable to see.
if [ -s "$TMP/jit" ]; then
  while IFS= read -r line; do
    key="${line%%=*}"
    want="$line"
    got=$(grep "^$key=" "$TMP/jit" | head -1)
    if [ "$got" != "$want" ]; then
      echo "FAIL(jit) $key: control=${want#*=} jit=${got#*=}"
      echo "       no --gpu in this arm -- this is a JIT miscompilation, not offload."
      echo "       known: docs/known-issues/jit/osr-miscompiles-cachecoherence-20260904.md"
      FAILS=$((FAILS + 1))
    fi
  done < <(grep '=' "$TMP/cpu")
else
  echo "FAIL: the compiled CPU arm produced no output"
  FAILS=$((FAILS + 1))
fi
while IFS= read -r line; do
  key="${line%%=*}"
  want="${line#*=}"
  got=$(grep "^$key=" "$TMP/gpu" | head -1)
  got="${got#*=}"
  if [ "$got" = "$want" ]; then
    echo "PASS $key"
  else
    echo "FAIL $key: control=$want gpu=$got"
    FAILS=$((FAILS + 1))
  fi
done < <(grep '=' "$TMP/cpu")

echo "=== summary ==="
if [ "$FAILS" = "0" ]; then
  echo "ALL RUNTIME STRESS SCENARIOS PASSED"
  exit 0
fi
echo "$FAILS SCENARIO(S) FAILED"
exit 1
