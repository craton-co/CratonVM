#!/usr/bin/env bash
# Can a JIT-COMPILED array writer leave the GPU residency cache stale?
#
# `offload_jit_gate` keeps array-writing methods OUT of the JIT under the
# default policy, because the IR pipeline's inline store has no hook to
# invalidate the input-residency cache from. The scan that decides this
# is a list of opcodes, and that list has to name every element type the
# cache can hold. On 2026-09-02 it did not: `short[]` and `byte[]` became
# cacheable while the scan still looked only for
# `iastore`/`lastore`/`fastore`/`dastore`, so their writers were compiled
# with their arrays device-resident. `jit_bastore` compounded it by never
# calling `input_cache::invalidate` at all.
#
# The fixture's int[] arm is the built-in CONTROL. `bumpI` contains
# `iastore`, which the scan does refuse, so the int checksum must be
# correct in the very same run in which short and byte are wrong. A
# failure on all three is a different bug from a failure on two.
#
# THE WRITER MUST BE HOT. At n=131072 over 400 rounds the defect does NOT
# reproduce -- `bumpS`/`bumpB` are called only 400 times and are never
# compiled, so the gate is never consulted and the run is vacuous. The
# defaults below (n=8192, 2000 rounds) make the writers hot enough to be
# compiled while keeping the arrays above `--gpu-min-work`. Raising n at
# the expense of rounds silently turns this back into a test of nothing.
#
# Usage:
#   CV=path/to/cratonvm.exe JDK=path/to/jdk bash bench-gpu/jit-writer-stale.sh [n] [rounds]
#
# Exit: 0 if the GPU arm matches HotSpot AND the kernels really offloaded.
set -u
export MSYS2_ARG_CONV_EXCL='*' MSYS_NO_PATHCONV=1

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"

CV="${CV:-$ROOT/target-gpu/release/cratonvm.exe}"
JDK="${JDK:-C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot}"
TG="${TG:-$ROOT/test_classes/gpu}"
N="${1:-8192}"
ROUNDS="${2:-2000}"
MINWORK="${MINWORK:-4096}"

if [ ! -f "$TG/GpuJitWriterStale.class" ]; then
  echo "compiling fixture into $TG"
  # Relative source path from $ROOT: javac is a Windows binary and
  # cannot open the MSYS-style "/c/..." that $ROOT expands to.
  (cd "$ROOT" && "$JDK/bin/javac" -d "$TG" "test_classes/gpu/GpuJitWriterStale.java") || exit 1
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

echo "=== jit array-writer vs residency cache (n=$N rounds=$ROUNDS) ==="
echo "CV=$CV"

"$JDK/bin/java" -cp "$TG" GpuJitWriterStale "$N" "$ROUNDS" 2>/dev/null \
    | grep '=' | tr -d '\r' > "$TMP/hs"
"$CV" --java-home "$JDK" -cp "$TG" --nojit GpuJitWriterStale "$N" "$ROUNDS" \
    2>/dev/null | grep '=' | tr -d '\r' > "$TMP/cpu"
RUST_LOG="cratonvm_vm::runtime::offload=info" CRATONVM_GPU_TRACE_BYTES=1 \
    "$CV" --java-home "$JDK" -cp "$TG" --gpu --gpu-min-work "$MINWORK" \
    GpuJitWriterStale "$N" "$ROUNDS" > "$TMP/gpu.out" 2> "$TMP/gpu.log"
grep '=' "$TMP/gpu.out" | tr -d '\r' > "$TMP/gpu"

for f in hs cpu gpu; do
  if [ ! -s "$TMP/$f" ]; then
    echo "FAIL: the '$f' arm produced no output"
    exit 1
  fi
done

FAILS=0

host=$(diff "$TMP/hs" "$TMP/cpu" | grep -c '^<')
if [ "$host" != "0" ]; then
  echo "FAIL: the CPU control already differs from HotSpot in $host line(s)."
  diff "$TMP/hs" "$TMP/cpu" | grep '^[<>]' | sed 's/^/       /'
  exit 1
fi
echo "control matches HotSpot"

# Engagement. A run where the kernels never dispatched cannot go stale,
# so it would pass no matter what the gate did.
echo
echo "--- engagement census (H2D dispatches per kernel) ---"
for k in scaleI scaleS scaleB; do
  c=$(grep -c "H2D=.*(GpuJitWriterStale\.$k" "$TMP/gpu.log" 2>/dev/null || true)
  if [ "${c:-0}" -gt 0 ]; then
    echo "  $k: $c transfer(s)"
  else
    echo "  $k: NEVER DISPATCHED -- nothing was resident, so this arm is vacuous"
    FAILS=$((FAILS + 1))
  fi
done

echo
echo "--- values ---"
while IFS= read -r line; do
  key="${line%%=*}"
  want="${line#*=}"
  got=$(grep "^$key=" "$TMP/gpu" | head -1)
  got="${got#*=}"
  if [ "$got" = "$want" ]; then
    echo "  PASS $key"
  else
    echo "  FAIL $key: control=$want gpu=$got"
    FAILS=$((FAILS + 1))
  fi
done < <(grep '=' "$TMP/cpu")

echo
echo "=== summary ==="
if [ "$FAILS" = "0" ]; then
  echo "NO STALE READS FROM A COMPILED ARRAY WRITER"
  exit 0
fi
echo "$FAILS CHECK(S) FAILED"
exit 1
