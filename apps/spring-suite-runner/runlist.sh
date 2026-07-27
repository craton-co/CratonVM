#!/usr/bin/env bash
# runlist.sh <classlist-file> <tag> [shards] — run an arbitrary set of FQCNs
# (one per line) through run-suite.sh, sharded, against this worktree's binary.
set -u
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"; cd "$HERE"
LIST="$1"; TAG="$2"; M="${3:-6}"
export SPRING="${SPRING:-/data/data/wt-springsuite8b-20260726/apps/spring-framework}"
export JDK25="${JDK25:-/home/victor/jdk25}"
export JDK25_WIN="${JDK25_WIN:-/home/victor/jdk25}"
export CRATONVM_BIN="${CRATONVM_BIN:-/data/data/wt-sprbuglist-20260727/localbin/cratonvm-sprbuglist-20260727.bin}"
export CRATONVM_DEFAULT_HEAP_MAX_MB="${CRATONVM_DEFAULT_HEAP_MAX_MB:-2048}"
BATCH="${BATCH:-4}"; HANG="${HANG:-600}"
BASE="$HERE/out/$TAG"; mkdir -p "$BASE"; export OUTROOT="$BASE"
# map FQCN -> module<TAB>class using meta/all-classes.tsv
awk -F'\t' 'NR==FNR{want[$1]=1;next} ($2 in want){print}' "$LIST" meta/all-classes.tsv > "$BASE/list.tsv"
echo "classes: $(wc -l < "$BASE/list.tsv") of $(grep -c . "$LIST") requested"
awk -F'\t' -v m="$M" -v b="$BASE" '{print > (b "/.shard-" ((NR-1)%m+1) ".tsv")}' "$BASE/list.tsv"
for i in $(seq 1 "$M"); do
  [ -s "$BASE/.shard-$i.tsv" ] || continue
  ( LISTFILE="$BASE/.shard-$i.tsv" ./run-suite.sh run --category custom --list "$BASE/.shard-$i.tsv"       --jdk real --jit on --batch "$BATCH" --batch-to "$HANG" --one-to "$HANG" --tag "sh$i" )       > "$BASE/sh$i.stdout.log" 2>&1 &
done
wait
cat "$BASE"/sh*-*/results.tsv > "$BASE/results.tsv" 2>/dev/null
cat "$BASE"/sh*-*/failcauses.log > "$BASE/failcauses.log" 2>/dev/null
cat "$BASE"/sh*-*/crashes.log > "$BASE/crashes.log" 2>/dev/null
echo '=== tally ==='; cut -f2 "$BASE/results.tsv" | sort | uniq -c
echo "-> $BASE"
