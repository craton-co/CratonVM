#!/usr/bin/env bash
# flag-ab.sh — interleaved A/B of ONE JIT flag, both arms from one binary.
#
# Same rules as tier-ab.sh: arms interleaved run-by-run (ABBA / BAAB by round),
# a CONTROL arm identical to A measured every round so its spread is the noise
# floor, medians, and an effect inside the floor reported as UNMEASURABLE.
# Checksums are compared across every run; a mismatch voids the result.
#
#   flag-ab.sh -Exe <vm> -Cp <dir> -Class <C> -Flag NAME -On 1 -Off 0 \
#              [-Base "VAR=V,VAR2=V2"] [-Rounds N] [-D k=v]...
set -uo pipefail

EXE=""; CP=""; CLASS=""; FLAG=""; ON="1"; OFF="0"; ROUNDS=7; JPROPS=(); BASE=""
while [ $# -gt 0 ]; do
  case "$1" in
    -Exe) EXE="$2"; shift 2 ;;
    -Cp) CP="$2"; shift 2 ;;
    -Class) CLASS="$2"; shift 2 ;;
    -Flag) FLAG="$2"; shift 2 ;;
    -On) ON="$2"; shift 2 ;;
    -Off) OFF="$2"; shift 2 ;;
    -Base) BASE="$2"; shift 2 ;;
    -Rounds) ROUNDS="$2"; shift 2 ;;
    -D) JPROPS+=("-D$2"); shift 2 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done
[ -n "$EXE" ] && [ -n "$CP" ] && [ -n "$CLASS" ] && [ -n "$FLAG" ] || {
  echo "need -Exe -Cp -Class -Flag" >&2; exit 2; }

IFS=',' read -r -a BASEENV <<< "${BASE:-}"

run_one() {
  local label="$1" val="$2" out ms acc
  out=$(env "${BASEENV[@]}" "$FLAG=$val" "$EXE" -Xmx8g -cp "$CP" "${JPROPS[@]}" "$CLASS" 2>/dev/null)
  ms=$(printf '%s' "$out" | grep -o 'ms=[0-9]*' | head -1 | cut -d= -f2)
  acc=$(printf '%s' "$out" | grep -o 'acc=[-0-9]*' | head -1 | cut -d= -f2)
  [ -n "$ms" ] || { echo "RUNFAIL $label" >&2; return 1; }
  printf '%s\t%s\t%s\n' "$label" "$ms" "$acc"
}

SAMPLES=$(mktemp); trap 'rm -f "$SAMPLES"' EXIT
echo "# $FLAG: A=$OFF (control C=$OFF)  B=$ON   base='${BASE:-none}'  class=$CLASS"

for r in $(seq 1 "$ROUNDS"); do
  if [ $((r % 2)) -eq 0 ]; then order="A B C B"; else order="B A B C"; fi
  for arm in $order; do
    case "$arm" in
      A|C) run_one "$arm" "$OFF" >>"$SAMPLES" ;;
      B)   run_one "$arm" "$ON"  >>"$SAMPLES" ;;
    esac
  done
  echo "  round $r" >&2
done

echo "--- samples ---"; cat "$SAMPLES"
echo "--- verdict ---"
awk -F'\t' '
  { v[$1]=v[$1]" "$2; n[$1]++; if (acc=="") acc=$3; else if (acc!=$3) mism=1 }
  function median(l,   a,c,i,j,t) {
    c=split(l,a," "); if(c==0) return -1
    for(i=1;i<=c;i++) for(j=i+1;j<=c;j++) if(a[i]+0>a[j]+0){t=a[i];a[i]=a[j];a[j]=t}
    return (c%2)?a[(c+1)/2]+0:(a[c/2]+a[c/2+1])/2.0
  }
  END{
    mA=median(v["A"]); mC=median(v["C"]); mB=median(v["B"])
    printf "A (flag off)  median %8.1f ms  n=%d\n", mA, n["A"]
    printf "C (control)   median %8.1f ms  n=%d\n", mC, n["C"]
    printf "B (flag on)   median %8.1f ms  n=%d\n", mB, n["B"]
    if(mA<=0||mC<=0){print "NO CONTROL"; exit}
    base=(mA+mC)/2.0
    floor=(mA>mC?mA-mC:mC-mA)/base*100.0
    eff=(mB-base)/base*100.0
    printf "noise floor (A vs C)     : %.1f%%\n", floor
    printf "effect (B vs mean(A,C))  : %+.1f%%   ratio %.3fx\n", eff, mB/base
    if(mism){print "*** CHECKSUM MISMATCH — RESULT VOID ***"; exit}
    ae=(eff<0?-eff:eff)
    if(ae<=floor) printf "VERDICT: UNMEASURABLE (%.1f%% effect inside a %.1f%% floor)\n", ae, floor
    else printf "VERDICT: flag ON is %s — %.3fx, above the %.1f%% floor\n", (eff<0?"FASTER":"SLOWER"), mB/base, floor
  }
' "$SAMPLES"
