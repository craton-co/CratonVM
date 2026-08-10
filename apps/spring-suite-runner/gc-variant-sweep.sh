#!/usr/bin/env bash
###############################################################################
# gc-variant-sweep.sh — run the whole Spring Framework index under each GC
# backend and diff the outcomes.
#
# One cratonvm binary (built with `--features zgc`) serves all three variants;
# the GC is selected at run time by a `-XX:+Use*GC` flag, so any difference
# between the columns is the collector and not the build.
#
# Usage (from anywhere):
#   SPRING=/path/to/spring-framework JDK25=/path/to/jdk-25 \
#   CRATONVM_BIN=/path/to/cratonvm ./gc-variant-sweep.sh [--shards N] [--tag NAME]
#
# Shards run in parallel WITHIN a variant; variants run one after another, so
# the three columns see the same machine load and stay comparable.
#
# Every run refuses to start unless run-suite.sh's `check-cp` passes — the
# 2026-08-10 sweep this script replaces was measured against a classpath whose
# entries did not exist, which turned 789 harness-induced NoClassDefFoundErrors
# into what looked like a 87% CratonVM pass rate. See
# docs/internal/fixed-suite-bugs/spring-testcp-jar-artifacts-20260810.md.
###############################################################################
set -u
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
OUTROOT="${OUTROOT:-$HERE/out}"

SHARDS="${SHARDS:-6}"
TAG="${TAG:-gcvariant}"
VARIANTS="${VARIANTS:-default g1 zgc}"
while [ $# -gt 0 ]; do
  case "$1" in
    --shards)   SHARDS="$2"; shift 2 ;;
    --tag)      TAG="$2"; shift 2 ;;
    --variants) VARIANTS="$2"; shift 2 ;;
    *) echo "unknown option: $1" >&2; exit 1 ;;
  esac
done

stamp="$(date +%Y%m%d-%H%M%S)"
BASE="$OUTROOT/$TAG-$stamp"
mkdir -p "$BASE"
echo "[sweep] base=$BASE shards=$SHARDS variants='$VARIANTS'"

# Prove the classpath ONCE, loudly, before spending hours on the sweep.
"$HERE/run-suite.sh" check-cp > "$BASE/classpath-check.log" 2>&1 || {
  grep -E '^(CP-INCOMPLETE|CP-MISSING-DUMP|\*\*\*)' "$BASE/classpath-check.log" | head -40
  echo "[sweep] ABORT: incomplete test classpath. Run: $HERE/run-suite.sh dumpcp" >&2
  exit 1
}
echo "[sweep] classpath verified complete"

gc_args() {
  case "$1" in
    default) echo "" ;;
    g1)      echo "-XX:+UseG1GC" ;;
    zgc)     echo "-XX:+UseZGC" ;;
    *) echo "unknown variant: $1" >&2; return 1 ;;
  esac
}

for v in $VARIANTS; do
  extra="$(gc_args "$v")" || exit 1
  echo "[sweep] === variant=$v  extra_vm_args='$extra' ==="
  t0=$(date +%s)
  for i in $(seq 1 "$SHARDS"); do
    ( EXTRA_VM_ARGS="$extra" OUTROOT="$BASE" \
      "$HERE/run-suite.sh" run --category all --shard "$i/$SHARDS" --tag "$v-s$i" \
      ) > "$BASE/$v-s$i.stdout.log" 2>&1 &
  done
  wait
  t1=$(date +%s)
  cat "$BASE"/$v-s*-jit-real-all-*/results.tsv > "$BASE/$v.results.tsv" 2>/dev/null
  echo "[sweep] variant=$v wall=$((t1-t0))s classes=$(wc -l < "$BASE/$v.results.tsv")"
done

# ------------------------------------------------------------------ summary ---
{
  echo "# Spring Framework full-index GC-variant sweep — $stamp"
  echo
  printf '| variant | %s | total |\n' "$(printf '%s | ' OK FAIL LOADERR TIMEOUT EMPTY CRASH ABEND)"
  printf '|---|--:|--:|--:|--:|--:|--:|--:|--:|\n'
  for v in $VARIANTS; do
    awk -F'\t' -v v="$v" '
      {c[$2]++; n++}
      END{ printf "| %s | %d | %d | %d | %d | %d | %d | %d | %d |\n", v,
           c["OK"]+0, c["FAIL"]+0, c["LOADERR"]+0, c["TIMEOUT"]+0, c["EMPTY"]+0,
           c["CRASH"]+0, c["ABEND"]+0, n }' "$BASE/$v.results.tsv"
  done
  echo
  echo "## failure causes (all variants pooled)"
  cat "$BASE"/*-s*-jit-real-all-*/failcauses.log 2>/dev/null \
    | sed -n 's/.*:: \([A-Za-z0-9_.$]*\(Exception\|Error\)\).*/\1/p' \
    | sort | uniq -c | sort -rn | head -30
} > "$BASE/SWEEP-SUMMARY.md"

cat "$BASE/SWEEP-SUMMARY.md"
echo "[sweep] done -> $BASE"
