#!/usr/bin/env bash
# tier-ab.sh — interleaved A/B of the baseline vs optimizing JIT tier on one probe.
#
# Methodology (the rules this repo's timing work already uses):
#   * arms are INTERLEAVED run-by-run, never blocked, so host drift hits both;
#   * a CONTROL arm (arm A run a second time, identical config) is measured
#     every round, and its spread is the noise floor;
#   * an effect inside the floor is reported as UNMEASURABLE, not as a number.
#
# Usage:
#   tier-ab.sh -Exe /abs/path/to/cratonvm -Cp <classes-dir> -Class FieldLoop \
#              [-Reps N] [-Rounds N] [-D probe.n=20000] ...
set -uo pipefail

EXE=""; CP=""; CLASS=""; ROUNDS=7; JPROPS=()
while [ $# -gt 0 ]; do
  case "$1" in
    -Exe)    EXE="$2"; shift 2 ;;
    -Cp)     CP="$2"; shift 2 ;;
    -Class)  CLASS="$2"; shift 2 ;;
    -Rounds) ROUNDS="$2"; shift 2 ;;
    -D)      JPROPS+=("-D$2"); shift 2 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done
[ -n "$EXE" ] && [ -n "$CP" ] && [ -n "$CLASS" ] || { echo "need -Exe -Cp -Class" >&2; exit 2; }

# One timed run. Prints the probe's own `ms=` figure, which times only the
# measured loop and excludes VM startup and warmup.
run_one() {
  local label="$1"; shift
  local out
  out=$(env "$@" "$EXE" -Xmx8g -cp "$CP" "${JPROPS[@]}" "$CLASS" 2>/dev/null)
  local ms acc
  ms=$(printf '%s' "$out" | grep -o 'ms=[0-9]*' | head -1 | cut -d= -f2)
  acc=$(printf '%s' "$out" | grep -o 'acc=[-0-9]*' | head -1 | cut -d= -f2)
  if [ -z "$ms" ]; then echo "RUNFAIL" >&2; return 1; fi
  printf '%s\t%s\t%s\n' "$label" "$ms" "$acc"
}

SAMPLES=$(mktemp)
trap 'rm -f "$SAMPLES"' EXIT

echo "# exe=$EXE class=$CLASS rounds=$ROUNDS props=${JPROPS[*]:-none}"
echo "# A=baseline(C2_SUPERSEDE=0)  B=optimizing(FORCE_C2=1)  C=control(=A)"

for r in $(seq 1 "$ROUNDS"); do
  # ABBA on even rounds, BAAB on odd, so ordering bias cancels across rounds.
  if [ $((r % 2)) -eq 0 ]; then order="A B C B"; else order="B A B C"; fi
  for arm in $order; do
    case "$arm" in
      A|C) run_one "$arm" CRATONVM_C2_SUPERSEDE=0 >>"$SAMPLES" ;;
      B)   run_one "$arm" CRATONVM_JIT_FORCE_C2=1 >>"$SAMPLES" ;;
    esac
  done
  echo "  round $r done" >&2
done

echo "--- samples ---"; cat "$SAMPLES"
echo "--- verdict ---"
awk -F'\t' '
  { v[$1] = v[$1] " " $2; n[$1]++; if (acc[$1] == "") acc[$1] = $3;
    else if (acc[$1] != $3) mismatch = 1 }
  function median(list,   a, c, i) {
    c = split(list, a, " "); if (c == 0) return -1
    for (i = 1; i <= c; i++) for (j = i + 1; j <= c; j++)
      if (a[i] + 0 > a[j] + 0) { t = a[i]; a[i] = a[j]; a[j] = t }
    return (c % 2) ? a[(c + 1) / 2] + 0 : (a[c / 2] + a[c / 2 + 1]) / 2.0
  }
  END {
    mA = median(v["A"]); mB = median(v["B"]); mC = median(v["C"])
    printf "A (baseline)   median %8.1f ms   n=%d\n", mA, n["A"]
    printf "C (control=A)  median %8.1f ms   n=%d\n", mC, n["C"]
    printf "B (optimizing) median %8.1f ms   n=%d\n", mB, n["B"]
    if (mA <= 0 || mC <= 0) { print "NO CONTROL"; exit }
    floor = (mA > mC ? mA - mC : mC - mA) / ((mA + mC) / 2.0) * 100.0
    eff   = (mB - (mA + mC) / 2.0) / ((mA + mC) / 2.0) * 100.0
    printf "noise floor (A vs C)      : %.1f%%\n", floor
    printf "effect  (B vs mean(A,C))  : %+.1f%%   ratio %.3fx\n", eff, mB / ((mA + mC) / 2.0)
    if (mismatch) print "*** CHECKSUM MISMATCH BETWEEN ARMS — result is void ***"
    ae = (eff < 0 ? -eff : eff)
    if (ae <= floor) printf "VERDICT: UNMEASURABLE (effect %.1f%% is inside the %.1f%% floor)\n", ae, floor
    else printf "VERDICT: %s — %.1fx, above the %.1f%% floor\n", (eff > 0 ? "OPTIMIZING IS SLOWER" : "OPTIMIZING IS FASTER"), mB / ((mA + mC) / 2.0), floor
  }
' "$SAMPLES"
