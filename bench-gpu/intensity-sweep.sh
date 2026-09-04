#!/usr/bin/env bash
# Arithmetic-intensity sweep: where does the GPU start paying, and does
# that crossover depend on ELEMENT WIDTH?
#
# ── THE QUESTION ──────────────────────────────────────────────────────────
#
# The 2026-09-02 pricing run measured ONE kernel shape -- a single
# multiply-subtract per element -- and found `long[]` a consistent ~1.55x
# LOSS in cold mode while `byte[]` won 2.75-3.33x. The obvious reading is
# that `--gpu-min-work` counts ELEMENTS, so an 8-byte-per-element kernel is
# admitted on the same terms as a 1-byte one, and at that transfer:compute
# ratio the wide type cannot pay for its own bytes.
#
# That reading may be right and that measurement cannot support it. One
# kernel at minimal arithmetic intensity is the worst case BY CONSTRUCTION:
# it is the point where transfer dominates most, so of course the widest
# type loses there. A threshold change would apply to every kernel.
#
# So this sweeps INTENSITY as well as width. Every kernel moves the SAME
# bytes; only the ops-per-element change (1, 4, 16). If the crossover is at
# 1-2 ops, a width-scaled threshold is real. If `long[]` already wins by 4,
# the original finding describes one degenerate shape and the default
# should not move.
#
# ── HOW IT MEASURES ───────────────────────────────────────────────────────
#
# Arms are `--gpu` vs no flag in the SAME binary. That is a within-binary
# A/B: nothing differs but the flag, which is a far cleaner shape than
# comparing two builds.
#
# ENGAGEMENT IS CHECKED, NOT ASSUMED. If `--gpu` silently declines and
# falls back, both arms run identical code and the cell reads "no
# difference" from a measurement that never touched the GPU. Every GPU run
# must report `gpu chunked writeback: taken=N` with N equal to the call
# count (20 warm-up + iters); a cell that misses is printed as NOT-ENGAGED
# and its ratio is suppressed.
#
# Arm order alternates by round so neither arm systematically gets the
# quiet moments. Gate this behind bench-gpu/wait-for-quiet.sh.
#
# KNOWN CONFOUND, recorded rather than hidden: under `--gpu` the gate
# denies JIT to the `run` dispatcher (`calls-eligible-kernel`), so the GPU
# arm pays interpreted dispatch that the CPU arm does not. At n=2^20 the
# kernel dominates, and it is honestly part of what `--gpu` costs, but it
# is a constant adder working against the GPU in every cell.
set -u
export MSYS2_ARG_CONV_EXCL='*' MSYS_NO_PATHCONV=1

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
CV="${CV:-$ROOT/cratonvm-gpu.exe}"
JDK="${JDK:-C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot}"
TG="${TG:-$ROOT/test_classes/gpu}"

N="${N:-1048576}"
ITERS="${ITERS:-50}"
ROUNDS="${ROUNDS:-3}"
TYPES="${TYPES:-I J D B}"
OPSET="${OPSET:-1 4 16}"
WARMUP=20                       # the fixture's own warm-up loop
EXPECT=$(( WARMUP + ITERS ))    # calls per run, hence expected writebacks

[ -x "$CV" ] || { echo "FATAL: missing $CV"; exit 1; }

# `test_classes/gpu/*.class` is gitignored (unlike `bench-gpu/*.class`,
# which is tracked), so a fresh worktree has the .java and no .class.
# Compile on demand rather than failing with a NoClassDefFoundError that
# does not say why.
if [ ! -f "$TG/GpuIntensitySweep.class" ]; then
  echo "compiling GpuIntensitySweep (class is gitignored) ..."
  # javac is a Windows exe and MSYS_NO_PATHCONV=1 is set above, so an
  # MSYS `/c/...` path reaches it verbatim and it reports "file not
  # found". cratonvm.exe happens to accept those; javac does not.
  tg_win="$TG"
  command -v cygpath >/dev/null 2>&1 && tg_win="$(cygpath -m "$TG")"
  "$JDK/bin/javac" -d "$tg_win" "$tg_win/GpuIntensitySweep.java" || {
    echo "FATAL: could not compile the fixture"; exit 1; }
fi

TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT

run_cell() {   # $1=arm(gpu|cpu) $2=type $3=ops -> "us sink engaged"
  local flag="" out us sink taken
  [ "$1" = "gpu" ] && flag="--gpu"
  out=$("$CV" $flag --java-home "$JDK" --Xmx 8g -cp "$TG" \
        GpuIntensitySweep "$2" "$N" "$ITERS" "$3" 2>&1)
  us=$(echo "$out"   | grep -oE 'us_per_call=[0-9]+' | head -1 | sed 's/.*=//')
  sink=$(echo "$out" | grep -oE 'sink=-?[0-9]+'      | head -1 | sed 's/.*=//')
  if [ "$1" = "gpu" ]; then
    taken=$(echo "$out" | grep -oE 'chunked writeback: taken=[0-9]+' | head -1 | sed 's/.*=//')
    taken=${taken:-0}
  else
    taken="$EXPECT"   # not applicable; the CPU arm cannot offload
  fi
  echo "${us:-NA} ${sink:-NA} ${taken}"
}

echo "=== arithmetic-intensity sweep: n=$N iters=$ITERS rounds=$ROUNDS ==="
echo "binary = $CV"
echo "expected writebacks per GPU run = $EXPECT (${WARMUP} warm-up + ${ITERS} timed)"
echo

for r in $(seq 1 "$ROUNDS"); do
  for t in $TYPES; do
    for o in $OPSET; do
      if [ $(( (r + o) % 2 )) -eq 0 ]; then order="gpu cpu"; else order="cpu gpu"; fi
      line=$(printf "r%-2s %-2s ops=%-3s" "$r" "$t" "$o")
      for arm in $order; do
        read -r us sink taken <<<"$(run_cell "$arm" "$t" "$o")"
        echo "$us"   >> "$TMP/$t.$o.$arm.us"
        echo "$sink" >> "$TMP/$t.$o.$arm.sink"
        [ "$arm" = "gpu" ] && echo "$taken" >> "$TMP/$t.$o.taken"
        line="$line  $(printf '%-3s=%7s us' "$arm" "$us")"
      done
      echo "$line"
    done
  done
done

echo
echo "=== results: speedup = cpu_us / gpu_us  (>1.00 means the GPU wins) ==="
printf "%-4s %-5s %10s %10s %9s  %s\n" type ops cpu_us gpu_us speedup notes
for t in $TYPES; do
  for o in $OPSET; do
    med() { sort -n "$1" | awk -v n="$(wc -l < "$1")" 'NR==int((n+1)/2){print}'; }
    c=$(med "$TMP/$t.$o.cpu.us"); g=$(med "$TMP/$t.$o.gpu.us")
    minTaken=$(sort -n "$TMP/$t.$o.taken" | head -1)
    notes=""
    [ "$minTaken" != "$EXPECT" ] && notes="NOT-ENGAGED(writebacks=$minTaken/$EXPECT)"
    if [ "$(sort -u "$TMP/$t.$o.cpu.sink")" != "$(sort -u "$TMP/$t.$o.gpu.sink")" ]; then
      notes="$notes SINK-MISMATCH"
    fi
    if [ -n "$notes" ]; then
      printf "%-4s %-5s %10s %10s %9s  %s\n" "$t" "$o" "$c" "$g" "--" "$notes"
    else
      printf "%-4s %-5s %10s %10s %9s\n" "$t" "$o" "$c" "$g" \
        "$(awk -v c="$c" -v g="$g" 'BEGIN{ if (g>0) printf "%.2fx", c/g; else print "NA" }')"
    fi
  done
done

echo
echo "Read: the crossover is the lowest ops where speedup passes 1.00."
echo "If it sits at the same ops for B and J, element width is NOT the"
echo "variable and --gpu-min-work should not be scaled by it."
