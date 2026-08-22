#!/usr/bin/env bash
# Summarise the three MySQL GC arms: per-arm status counts, the union of
# non-PASS classes, and which arms each one is non-PASS on.
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
STAMP="${STAMP:-20260822-3gc-mysql}"
declare -A ARMS=()
echo "| GC | PASS | FAIL | HANG | CRASH | NOTESTS | ABORTED | total |"
echo "|---|---:|---:|---:|---:|---:|---:|---:|"
for gc in default g1 generational; do
  f=$(ls "$HERE"/runs/mysql-$gc-$STAMP/*/on-real/results.tsv 2>/dev/null | head -1)
  [ -f "$f" ] || { echo "| $gc | (no results.tsv) |"; continue; }
  ARMS[$gc]="$f"
  awk -F'\t' -v gc="$gc" 'NR>1{c[$3]++; t++} END{printf "| %s | %d | %d | %d | %d | %d | %d | %d |\n", gc, c["PASS"], c["FAIL"], c["HANG"], c["CRASH"], c["NOTESTS"], c["ABORTED"], t}' "$f"
done
echo
echo "Non-PASS classes (arm list, then status per arm):"
for gc in "${!ARMS[@]}"; do awk -F'\t' -v gc="$gc" 'NR>1 && $3!="PASS" && $3!="NOTESTS" {print $2"\t"gc"="$3}' "${ARMS[$gc]}"; done \
 | sort | awk -F'\t' '{if($1!=p){if(p!="")print p"  "s; p=$1; s=$2}else s=s" "$2} END{if(p!="")print p"  "s}'
echo
echo "NOTESTS count is the harness's no-@@RESULT bucket (abstract/base classes etc.)."
