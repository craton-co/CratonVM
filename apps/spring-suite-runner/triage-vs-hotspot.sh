#!/usr/bin/env bash
###############################################################################
# triage-vs-hotspot.sh — split a run's non-passing classes into "CratonVM
# defect" and "fails on HotSpot too".
#
#   ./triage-vs-hotspot.sh <results.tsv> [--tag NAME]
#
# Takes the non-OK rows of a CratonVM results.tsv, re-runs EXACTLY those classes
# under HotSpot java with the same harness, classpath and JVM args, and joins
# the two on class name. A class that fails both ways is not evidence about
# CratonVM — it is the suite's own state (a missing external service, an
# environment assumption, a genuinely broken upstream test).
#
# HotSpot is the control. Without it a failure list is just a list; the 2026-08-10
# sweep this tool was written for spent its whole budget on 789 failures that
# turned out to be a harness classpath bug, and no control run was taken that
# would have shown HotSpot failing identically.
#
# Output: <outdir>/triage.tsv with columns
#   class  cratonvm-status  hotspot-status  verdict
# where verdict is one of
#   CRATONVM-DEFECT   cratonvm non-OK, HotSpot OK       <- the actionable set
#   BOTH-FAIL         non-OK on both                    <- not a CratonVM signal
#   HOTSPOT-ONLY      cratonvm OK, HotSpot non-OK       <- suspect the control
###############################################################################
set -u
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RESULTS="${1:-}"
[ -s "$RESULTS" ] || { echo "usage: $0 <results.tsv> [--tag NAME]" >&2; exit 1; }
shift
TAG="triage"
while [ $# -gt 0 ]; do
  case "$1" in
    --tag) TAG="$2"; shift 2 ;;
    *) echo "unknown option: $1" >&2; exit 1 ;;
  esac
done

META="$HERE/meta"
ALLIDX="$META/all-classes.tsv"
[ -s "$ALLIDX" ] || { echo "no class index — run: $HERE/run-suite.sh discover" >&2; exit 1; }

stamp="$(date +%Y%m%d-%H%M%S)"
OUT="${OUTROOT:-$HERE/out}/$TAG-$stamp"
mkdir -p "$OUT"

# non-OK classes, in index order, as a module<TAB>class list the runner accepts
awk -F'\t' '$2!="OK"{print $1"\t"$2}' "$RESULTS" | sort -u > "$OUT/nonok.tsv"
awk -F'\t' 'NR==FNR{s[$1]=$2; next} ($2 in s){print}' "$OUT/nonok.tsv" "$ALLIDX" > "$OUT/list.tsv"
n="$(wc -l < "$OUT/list.tsv")"
echo "[triage] $n non-passing classes -> HotSpot control"
[ "$n" -eq 0 ] && { echo "[triage] nothing to do"; exit 0; }

OUTROOT="$OUT" "$HERE/run-suite.sh" hotspot --list "$OUT/list.tsv" --tag hs \
  > "$OUT/hotspot.stdout.log" 2>&1
HS="$(ls -d "$OUT"/hs-hotspot-custom-*/results.tsv 2>/dev/null | head -1)"
[ -s "$HS" ] || { echo "[triage] HotSpot control produced no results — see $OUT/hotspot.stdout.log" >&2; exit 1; }

{
  printf 'class\tcratonvm\thotspot\tverdict\n'
  awk -F'\t' '
    NR==FNR { hs[$1]=$2; next }
    {
      h = ($1 in hs) ? hs[$1] : "NORUN"
      v = ($2=="OK" && h!="OK") ? "HOTSPOT-ONLY" : (h=="OK" ? "CRATONVM-DEFECT" : "BOTH-FAIL")
      printf "%s\t%s\t%s\t%s\n", $1, $2, h, v
    }' "$HS" "$OUT/nonok.tsv"
} > "$OUT/triage.tsv"

echo "[triage] verdicts:"
awk -F'\t' 'NR>1{c[$4]++} END{for(k in c) printf "  %-16s %d\n", k, c[k]}' "$OUT/triage.tsv"
echo "[triage] -> $OUT/triage.tsv"
