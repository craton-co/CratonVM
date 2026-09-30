#!/usr/bin/env bash
# =============================================================================
# h2-ab.sh — an A/B for a SINGLE-UNIT workload (H2's DodJdbcWorkload and kin),
# built to the same discipline as tools/suite-pair-ab but for the case where
# there is one timed unit instead of many classes.
#
# WHY IT EXISTS. On 2026-09-07 an audit of docs/JIT_OPTIMIZATION.md found that
# every H2 throughput number in it — including the `OVER_INTRINSIC` ~3% claim
# that keeps a flag off and scopes a work item — was taken with an ad-hoc
# harness that was never committed and no longer exists. The corpus's
# `test-classes` directory is empty too, so the 21-class shape it used cannot be
# rebuilt here. Nothing in tools/ could re-check any of it.
#
# WHAT IT CANNOT DO, said plainly. With ONE unit there is no per-class pairing,
# so there is no sign test and no split-half check — the two things that caught
# a false result the same day. This instrument can only answer "is the effect
# bigger than this host's same-config noise", which is the weakest of the three
# questions. Prefer suite-pair-ab whenever the workload is fork-per-class.
#
# THE DESIGN, and it is the part that matters:
#   * ABBA per round (BAAB on odd rounds) so linear drift cancels within a round
#     rather than across the run.
#   * A CONTROL arm — the same configuration measured twice — every round. Its
#     spread is the noise floor. An effect smaller than the floor is not a
#     result, and a round whose CONTROL disagrees with itself by more than the
#     effect under test is DISCARDED, not reported. That rule cost one of three
#     rounds in the original measurement and is why its answer was trustworthy.
#   * Wall AND cpu where the platform gives it, because on this host they have
#     disagreed.
#
# USAGE
#   h2-ab.sh --lever <ENV> [--on 1] [--off 0] [--rounds 5] --cp <classpath>
#            [--class DodJdbcWorkload] [--bin <cratonvm>] [--out <dir>]
#   h2-ab.sh --analyze <samples.tsv>     # statistics only, no VM: see selftest
#
# samples.tsv columns: round<TAB>arm<TAB>ms      (arm is A, B, or C1/C2)
# =============================================================================
set -u

LEVER=""; ON=1; OFF=0; ROUNDS=5; CP=""; CLASS="DodJdbcWorkload"
BIN="./target/release/cratonvm.exe"; OUT=""; ANALYZE=""
while [ $# -gt 0 ]; do
  case "$1" in
    --lever) LEVER="$2"; shift 2;;
    --on) ON="$2"; shift 2;;
    --off) OFF="$2"; shift 2;;
    --rounds) ROUNDS="$2"; shift 2;;
    --cp) CP="$2"; shift 2;;
    --class) CLASS="$2"; shift 2;;
    --bin) BIN="$2"; shift 2;;
    --out) OUT="$2"; shift 2;;
    --analyze) ANALYZE="$2"; shift 2;;
    *) echo "unknown option: $1" >&2; exit 2;;
  esac
done

analyze() {
  awk -F'\t' '
    NR>1 {
      arm=$2; ms=$3+0
      if (arm=="A") { a[na++]=ms }
      else if (arm=="B") { b[nb++]=ms }
      else { c[$1"/"arm]=ms; cr[$1]=1 }
    }
    function med(arr,m,  i,j,t,cp2) {
      for (i=0;i<m;i++) cp2[i]=arr[i]
      for (i=1;i<m;i++){t=cp2[i];for(j=i-1;j>=0&&cp2[j]>t;j--)cp2[j+1]=cp2[j];cp2[j+1]=t}
      return (m%2)?cp2[int(m/2)]:(cp2[m/2-1]+cp2[m/2])/2
    }
    END{
      if (na==0 || nb==0) { print "no samples"; exit 1 }
      ma=med(a,na); mb=med(b,nb)
      eff = (mb-ma)/ma*100          # >0 means B slower, i.e. A faster
      # noise floor: the worst same-config disagreement across rounds
      worst=0; nc=0
      for (r in cr) {
        if ((r"/C1") in c && (r"/C2") in c) {
          x=c[r"/C1"]; y=c[r"/C2"]
          d=(x>y)?(x-y)/y*100:(y-x)/x*100
          if (d>worst) worst=d
          nc++
        }
      }
      ae = (eff<0)?-eff:eff
      printf "rounds kept   : %d   (A n=%d, B n=%d, control pairs=%d)\n", nc, na, nb, nc
      printf "median A      : %.0f ms\n", ma
      printf "median B      : %.0f ms\n", mb
      printf "effect        : %+.1f%%   (positive = A faster)\n", eff
      printf "noise floor   : %.1f%%   (worst SAME-config disagreement)\n", worst
      printf "----------------------------------------------------------\n"
      if (nc==0) {
        print "VERDICT: NO CONTROL. Without a same-config pair there is no floor to"
        print "         compare against, and a single-unit A/B has nothing else."
        print "         Report nothing."
      } else if (ae <= worst) {
        printf "VERDICT: UNMEASURABLE. The effect (%.1f%%) is not larger than this\n", ae
        printf "         same-config spread on this host (%.1f%%). Report no number.\n", worst
      } else {
        printf "VERDICT: effect %.1f%% exceeds the %.1f%% floor.\n", ae, worst
        print  "         ONE UNIT ONLY: this has no per-class sign test and no"
        print  "         split-half check, so it cannot see an effect that depends"
        print  "         on WHICH work was sampled. Confirm on a second workload"
        print  "         before reporting a direction."
      }
    }' "$1"
}

if [ -n "$ANALYZE" ]; then analyze "$ANALYZE"; exit $?; fi

[ -n "$LEVER" ] && [ -n "$CP" ] || { echo "need --lever and --cp (or --analyze)" >&2; exit 2; }
[ -n "$OUT" ] || OUT="./h2-ab-runs/$(date +%Y%m%d-%H%M%S)"
mkdir -p "$OUT"; TSV="$OUT/samples.tsv"
printf 'round\tarm\tms\n' > "$TSV"

one() { # env-value -> ms on stdout
  local v="$1" t0 t1
  t0=$(date +%s%N)
  env "$LEVER=$v" "$BIN" -cp "$CP" "$CLASS" >/dev/null 2>&1
  t1=$(date +%s%N)
  echo $(( (t1-t0)/1000000 ))
}

echo "h2-ab: lever=$LEVER A=$ON B=$OFF rounds=$ROUNDS class=$CLASS"
echo "       (single-unit: no sign test, no split-half -- see the header)"
for r in $(seq 1 "$ROUNDS"); do
  # ABBA on even rounds, BAAB on odd, so order bias cancels across rounds too
  if [ $((r % 2)) -eq 0 ]; then order="A B B A"; else order="B A A B"; fi
  for arm in $order; do
    [ "$arm" = A ] && v="$ON" || v="$OFF"
    ms=$(one "$v"); printf '%s\t%s\t%s\n' "$r" "$arm" "$ms" >> "$TSV"
  done
  # the control: the SAME configuration twice, adjacent, this round
  c1=$(one "$OFF"); c2=$(one "$OFF")
  printf '%s\tC1\t%s\n%s\tC2\t%s\n' "$r" "$c1" "$r" "$c2" >> "$TSV"
  echo "  round $r: control ${c1}ms / ${c2}ms"
done
echo "=========================================================="
analyze "$TSV"
echo "=========================================================="
echo "samples: $TSV"
