#!/usr/bin/env bash
# Where does offload STOP paying? Sweep the element count down until the
# GPU loses, per element width.
#
# ── WHY THIS EXISTS ───────────────────────────────────────────────────────
#
# `bench-gpu/intensity-sweep.sh` (2026-09-04) swept arithmetic intensity at
# n=2^20 and found the GPU never loses -- the worst cell in a 12-cell grid
# was 1.00x. That answered the question it was built for (do NOT scale
# `--gpu-min-work` by element width) and could not answer the next one: a
# grid in which nothing loses contains no refusal boundary, so it cannot
# site a threshold.
#
# This sweeps the other axis. `ops` is held at the LOWEST intensity, which
# is the worst case for the GPU and therefore the conservative place to
# site an admission threshold, and `n` walks down through and below the
# current default.
#
# ── THE LEVER ─────────────────────────────────────────────────────────────
#
# `--gpu-min-work` defaults to 4096 ELEMENTS, so below that the VM refuses
# on size and the cell would report NOT-ENGAGED rather than "the GPU lost".
# The GPU arm therefore runs with `--gpu-min-work 1`: nothing is refused
# for being small, and the DATA says where the threshold belongs
# independently of where it currently sits.
#
# An engagement failure that survives that is a finding in its own right --
# something other than size refused -- which is why the cell prints
# NOT-ENGAGED with its ratio suppressed instead of a number.
#
# The counter is `gpu dispatch memo` (served + re-derived), which is
# emitted at every size. The first draft used `chunked writeback: taken=`,
# which only appears once chunking engages (CHUNK_MIN_ELEMS = 1<<19) and
# is therefore ABSENT across most of this sweep -- it reported 0/120 at
# n=1024 on a run that had in fact dispatched all 70 calls and allocated
# 50 device buffers. An instrument that cannot fire in the regime under
# test reads exactly like a refusal.
#
# ── RESOLUTION ────────────────────────────────────────────────────────────
#
# Uses `ns_per_call`, added to the fixture for this sweep. `us_per_call` is
# integer microseconds: fine at n=2^20 (~2000 units), useless at n=2^10
# where a call costs one or two microseconds and the quotient quantises to
# 1-2. Reading a crossover off a quantised metric would invent one.
#
# Arms are `--gpu` vs no flag in the SAME binary; order alternates by round.
# Gate behind bench-gpu/wait-for-quiet.sh.
set -u
export MSYS2_ARG_CONV_EXCL='*' MSYS_NO_PATHCONV=1

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
CV="${CV:-$ROOT/cratonvm-gpu.exe}"
JDK="${JDK:-C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot}"
TG="${TG:-$ROOT/test_classes/gpu}"

OPS="${OPS:-1}"
ITERS="${ITERS:-200}"
ROUNDS="${ROUNDS:-3}"
TYPES="${TYPES:-I J D B}"
NSET="${NSET:-1048576 262144 65536 16384 8192 4096 2048 1024}"
MINWORK="${MINWORK:-1}"
LOSS_AT="${LOSS_AT:-0.90}"   # speedup below this counts as a real loss
WARMUP=20
EXPECT=$(( WARMUP + ITERS ))

[ -x "$CV" ] || { echo "FATAL: missing $CV"; exit 1; }

if [ ! -f "$TG/GpuIntensitySweep.class" ]; then
  echo "compiling GpuIntensitySweep (class is gitignored) ..."
  tg_win="$TG"
  command -v cygpath >/dev/null 2>&1 && tg_win="$(cygpath -m "$TG")"
  "$JDK/bin/javac" -d "$tg_win" "$tg_win/GpuIntensitySweep.java" || {
    echo "FATAL: could not compile the fixture"; exit 1; }
fi

TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT

run_cell() {   # $1=arm $2=type $3=n -> "ns sink taken"
  local out ns sink taken
  if [ "$1" = "gpu" ]; then
    out=$("$CV" --gpu --gpu-min-work "$MINWORK" --java-home "$JDK" --Xmx 8g \
          -cp "$TG" GpuIntensitySweep "$2" "$3" "$ITERS" "$OPS" 2>&1)
    # Dispatch count, NOT `chunked writeback: taken=`. That line only
    # exists when chunking engages, which needs CHUNK_MIN_ELEMS = 1<<19 --
    # so it is absent for every n this sweep cares about, and using it
    # would mark the whole small-n half NOT-ENGAGED and "prove" a refusal
    # that is not there. `gpu dispatch memo` is emitted at every size:
    # served + re-derived is the number of offloaded calls.
    local served rederived
    served=$(echo "$out"    | grep -oE 'resolutions served=[0-9]+'  | head -1 | sed 's/.*=//')
    rederived=$(echo "$out" | grep -oE 're-derived=[0-9]+'          | head -1 | sed 's/.*=//')
    taken=$(( ${served:-0} + ${rederived:-0} ))
  else
    out=$("$CV" --java-home "$JDK" --Xmx 8g \
          -cp "$TG" GpuIntensitySweep "$2" "$3" "$ITERS" "$OPS" 2>&1)
    taken="$EXPECT"
  fi
  ns=$(echo "$out"   | grep -oE 'ns_per_call=[0-9]+' | head -1 | sed 's/.*=//')
  sink=$(echo "$out" | grep -oE 'sink=-?[0-9]+'      | head -1 | sed 's/.*=//')
  echo "${ns:-NA} ${sink:-NA} ${taken}"
}

echo "=== crossover sweep: ops=$OPS iters=$ITERS rounds=$ROUNDS --gpu-min-work=$MINWORK ==="
echo "binary = $CV"
echo "expected writebacks per GPU run = $EXPECT"
echo

for r in $(seq 1 "$ROUNDS"); do
  for t in $TYPES; do
    for n in $NSET; do
      if [ $(( r % 2 )) -eq 0 ]; then order="gpu cpu"; else order="cpu gpu"; fi
      line=$(printf "r%-2s %-2s n=%-8s" "$r" "$t" "$n")
      for arm in $order; do
        read -r ns sink taken <<<"$(run_cell "$arm" "$t" "$n")"
        echo "$ns"   >> "$TMP/$t.$n.$arm.ns"
        echo "$sink" >> "$TMP/$t.$n.$arm.sink"
        [ "$arm" = "gpu" ] && echo "$taken" >> "$TMP/$t.$n.taken"
        line="$line  $(printf '%-3s=%9s ns' "$arm" "$ns")"
      done
      echo "$line"
    done
  done
done

med() { sort -n "$1" | awk -v n="$(wc -l < "$1")" 'NR==int((n+1)/2){print}'; }

echo
echo "=== speedup = cpu_ns / gpu_ns  (>1.00 the GPU wins; <1.00 it loses) ==="
printf "%-4s %10s %12s %12s %9s  %s\n" type n cpu_ns gpu_ns speedup notes
for t in $TYPES; do
  cross=""
  for n in $NSET; do
    c=$(med "$TMP/$t.$n.cpu.ns"); g=$(med "$TMP/$t.$n.gpu.ns")
    mt=$(sort -n "$TMP/$t.$n.taken" | head -1)
    notes=""
    [ "$mt" != "$EXPECT" ] && notes="NOT-ENGAGED($mt/$EXPECT)"
    [ "$(sort -u "$TMP/$t.$n.cpu.sink")" != "$(sort -u "$TMP/$t.$n.gpu.sink")" ] && notes="$notes SINK-MISMATCH"
    if [ -n "$notes" ]; then
      printf "%-4s %10s %12s %12s %9s  %s\n" "$t" "$n" "$c" "$g" "--" "$notes"
    else
      sp=$(awk -v c="$c" -v g="$g" 'BEGIN{ if (g>0) printf "%.2f", c/g; else print "0" }')
      printf "%-4s %10s %12s %12s %8sx\n" "$t" "$n" "$c" "$g" "$sp"
      # A CLEAR loss, not merely <1.00. Round-to-round noise on this box
      # is ~10-17% (measured 2026-09-04, docs/gpu/cuda-oxide-evaluation.md),
      # and the largest sizes sit at parity -- n=2^20 for `long[]` read
      # 1.04x, 0.98x and 0.85x on three separate runs. A bare `< 1.00`
      # test would name whichever of those happened to land low as the
      # crossover. LOSS_AT is the margin that puts a cell outside the
      # noise; cells between it and 1.00 print as parity in the curve.
      awk -v s="$sp" -v t="$LOSS_AT" 'BEGIN{ exit !(s < t) }' && [ -z "$cross" ] && cross="$n"
    fi
  done
  if [ -n "$cross" ]; then
    echo "  -> $t: first CLEAR loss (< ${LOSS_AT}x) at n=$cross"
  else
    echo "  -> $t: no clear loss (< ${LOSS_AT}x) down to n=$(echo $NSET | awk '{print $NF}')"
  fi
  echo
done
echo "The admission threshold belongs at the largest n that still loses,"
echo "per width. If that n is the same for every width, --gpu-min-work"
echo "should stay a plain element count (current default: 4096)."
