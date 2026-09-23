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
ROUNDS="${2:-6000}"
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

# How many times did a COMPILED array store hit a device-resident array?
#
# AUDIT 2026-09-04. This arm passed at `ROUNDS=2000` with the barrier
# firing ZERO times, because `bump*` is called 2000 times for 128
# iterations each and never gets hot enough to compile. Every store ran
# interpreted, every interpreted store evicts, and the fixture proved
# nothing about the compiled tier -- which is the only tier it exists to
# test. At 6000 rounds all four `bump*` compile and the barrier fires
# ~5.5k times.
#
# So the drain count is checked, not just the checksums: a green run
# with `drains=0` is a run that tested nothing.
DRAINS=0
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

DRAINS=$(grep -o 'compiled-write drains=[0-9]*' "$TMP/gpu.log" | head -1 | cut -d= -f2)
DRAINS="${DRAINS:-0}"
RELEASED=$(grep -o 'array-writer-behind-barrier=[0-9]*' "$TMP/gpu.log" | head -1 | cut -d= -f2)
RELEASED="${RELEASED:-0}"

# Engagement, part 1: did the BARRIER fire? See the note above `DRAINS`.
echo
echo "--- engagement census (compiled-tier barrier) ---"
echo "  array writers admitted behind the barrier: $RELEASED"
echo "  compiled stores that hit a resident array: $DRAINS"
if [ "$DRAINS" = "0" ]; then
  echo "  ZERO -- no compiled store ever hit a cached array, so the"
  echo "  checksums below cannot distinguish a working barrier from a"
  echo "  missing one. Raise ROUNDS until the bump* writers compile."
  FAILS=$((FAILS + 1))
fi
echo
# Engagement, part 2. A run where the kernels never dispatched cannot go
# stale, so it would pass no matter what the gate or the barrier did.
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
