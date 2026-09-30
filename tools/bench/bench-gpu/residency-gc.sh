#!/usr/bin/env bash
# Does the GPU input-residency cache survive a moving collector, for the
# element widths that became offloadable on 2026-09-02?
#
# The cache is keyed by `ObjectRef` -- a raw heap address -- so every
# relocation has to be followed by `input_cache::remap_and_sweep`
# rewriting the keys. `CachedBuffer::I16`/`I8` entries only started
# appearing in that cache when short[]/byte[] became offloadable, so
# they have never been through a relocation before. The remap is generic
# over entries and should carry them; this measures it rather than
# assuming, because "should" is the word that preceded the gap those two
# types were already in.
#
# A mis-keyed entry gives WRONG VALUES, not a crash: a submit after a
# relocation reads the device buffer of the array's old address, or of
# whatever now occupies it.
#
# Run once PER COLLECTOR. A pass under one collector says nothing about
# another -- ZGC-real, G1 and Generational relocate on different
# schedules and through different code, and only some configurations
# move an object at all.
#
#   -XX:+UseZGC             the default
#   -XX:+UseG1GC            region-based, evacuating
#   -XX:+UseGenerationalGC  semi-space young gen: relocates every survivor
#
# THREE arms per collector, plus a census:
#
#   HotSpot            the oracle (collector-independent: the answers
#                      are a pure function of the inputs)
#   cratonvm --nojit   the CONTROL on that same collector
#   cratonvm --gpu     the arm under test
#
# The census matters as much as the values. If the short[]/byte[]
# kernels do not dispatch, the GPU arm is the interpreter and the run
# proves nothing about the cache -- which is exactly how the
# short[]/byte[] marshalling gap survived until 2026-09-02.
#
# Usage:
#   CV=path/to/cratonvm.exe JDK=path/to/jdk bash bench-gpu/residency-gc.sh [n] [rounds]
#
# Windows note: CV/JDK/TG must be Windows-style (C:/...).
#
# Exit: 0 if every collector's GPU arm matches its control AND the
# short[]/byte[] kernels really dispatched under each.
set -u
export MSYS2_ARG_CONV_EXCL='*' MSYS_NO_PATHCONV=1

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"

CV="${CV:-$ROOT/target-gpu/release/cratonvm.exe}"
JDK="${JDK:-C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot}"
TG="${TG:-$ROOT/test_classes/gpu}"
# The defaults matter and are not arbitrary. A relocation only happens
# to an array that is (a) small enough not to be allocated straight into
# a non-moving space and (b) young while it is resident in the cache, and
# a collection only happens at all if the heap is small enough to fill.
# With the obvious defaults -- a 131072-element array and no -Xmx -- this
# script drove ZERO collections; with -Xmx alone it drove 25 collections
# that never moved a cached array. Both read as a clean pass.
N="${1:-1024}"
ROUNDS="${2:-60}"
# Small heap so collections actually happen; low min-work so a
# 1024-element array still clears the offload threshold.
HEAP="${HEAP:--Xmx64m}"
MINWORK="${MINWORK:-64}"

if [ ! -f "$TG/GpuResidencyGc.class" ]; then
  echo "compiling fixture into $TG"
  # Relative source path from $ROOT: javac is a Windows binary and
  # cannot open the MSYS-style "/c/..." that $ROOT expands to.
  (cd "$ROOT" && "$JDK/bin/javac" -d "$TG" "test_classes/gpu/GpuResidencyGc.java") || exit 1
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

echo "=== gpu residency across a moving collector (n=$N rounds=$ROUNDS) ==="
echo "CV=$CV"

"$JDK/bin/java" -cp "$TG" GpuResidencyGc 0 "$N" "$ROUNDS" 2>/dev/null \
    | grep '=' | tr -d '\r' > "$TMP/hs"
if [ ! -s "$TMP/hs" ]; then
  echo "FAIL: the HotSpot oracle produced no output"
  exit 1
fi

FAILS=0

for gc in "-XX:+UseZGC" "-XX:+UseG1GC" "-XX:+UseGenerationalGC"; do
  name="${gc#-XX:+Use}"
  echo
  echo "───── $name ─────"

  "$CV" --java-home "$JDK" -cp "$TG" $HEAP --gpu-min-work "$MINWORK" "$gc" --nojit GpuResidencyGc 0 "$N" "$ROUNDS" \
      2>/dev/null | grep '=' | tr -d '\r' > "$TMP/cpu.$name"
  RUST_LOG="cratonvm_vm::runtime::offload=info" CRATONVM_GPU_TRACE_BYTES=1 \
      "$CV" --java-home "$JDK" -cp "$TG" $HEAP --gpu-min-work "$MINWORK" "$gc" --gpu GpuResidencyGc 0 "$N" "$ROUNDS" \
      > "$TMP/gpu.$name.out" 2> "$TMP/gpu.$name.log"
  grep '=' "$TMP/gpu.$name.out" | tr -d '\r' > "$TMP/gpu.$name"

  if [ ! -s "$TMP/cpu.$name" ] || [ ! -s "$TMP/gpu.$name" ]; then
    echo "  FAIL: an arm produced no output under $name"
    FAILS=$((FAILS + 1))
    continue
  fi

  # The control must match the oracle before the device arm is read.
  host=$(diff "$TMP/hs" "$TMP/cpu.$name" | grep -c '^<')
  if [ "$host" != "0" ]; then
    echo "  FAIL: the CPU control under $name already differs from HotSpot"
    echo "        in $host line(s). A host-side or collector defect looks"
    echo "        exactly like a residency one from here."
    diff "$TMP/hs" "$TMP/cpu.$name" | grep '^[<>]' | sed 's/^/         /'
    FAILS=$((FAILS + 1))
    continue
  fi
  echo "  control matches HotSpot"

  # Engagement, part 1: a cached array must actually have been MOVED.
  # Values matching while nothing relocated says only that the remap was
  # never asked to do anything. `re-keyed` counts cache entries whose key
  # the collector changed.
  #
  # This check exists because the only previous account of the remap was
  # a `tracing::debug!`, and `tracing` is built with `max_level_info`, so
  # it is compiled out of every release build and reads zero no matter
  # what happened. See `types::gpu_residency_census`.
  census=$(grep "gpu residency across GC" "$TMP/gpu.$name.log" | head -1)
  rekeyed=$(printf '%s' "$census" | grep -oE "re-keyed=[0-9]+" | grep -oE "[0-9]+")
  cols=$(printf '%s' "$census" | grep -oE "collections=[0-9]+" | grep -oE "[0-9]+")
  if [ -z "${rekeyed:-}" ]; then
    echo "  FAIL: the cache saw no collection at all under $name (no census"
    echo "        line). Nothing about the remap was exercised; shrink HEAP"
    echo "        or raise ROUNDS."
    FAILS=$((FAILS + 1))
  elif [ "$rekeyed" -eq 0 ]; then
    echo "  FAIL: $cols collection(s) under $name, but NOT ONE moved a cached"
    echo "        array (re-keyed=0). The remap path did not run, so this arm"
    echo "        proves nothing -- the vacuous pass this check refuses."
    FAILS=$((FAILS + 1))
  else
    echo "  relocation: $cols collection(s), $rekeyed cache entr(ies) re-keyed"
  fi

  # Engagement: without a short[] and a byte[] dispatch this run says
  # nothing about I16/I8 cache entries surviving a relocation.
  for k in scaleS scaleB scaleI; do
    c=$(grep -c "H2D=.*(GpuResidencyGc\.$k" "$TMP/gpu.$name.log" 2>/dev/null || true)
    if [ "${c:-0}" -gt 0 ]; then
      echo "  census $k: $c transfer(s)"
    else
      echo "  census $k: NEVER DISPATCHED -- this collector's arm is vacuous"
      FAILS=$((FAILS + 1))
    fi
  done

  while IFS= read -r line; do
    key="${line%%=*}"
    want="${line#*=}"
    got=$(grep "^$key=" "$TMP/gpu.$name" | head -1)
    got="${got#*=}"
    if [ "$got" = "$want" ]; then
      echo "  PASS $key"
    else
      echo "  FAIL $key: control=$want gpu=$got"
      FAILS=$((FAILS + 1))
    fi
  done < <(grep '=' "$TMP/cpu.$name")
done

echo
echo "=== summary ==="
if [ "$FAILS" = "0" ]; then
  echo "RESIDENCY SURVIVES RELOCATION ON EVERY COLLECTOR"
  exit 0
fi
echo "$FAILS CHECK(S) FAILED"
exit 1
