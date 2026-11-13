#!/usr/bin/env bash
set -euo pipefail

row=${1:?row}; kind=${2:?hs|cvm}; mode=${3:-jdk-only}; shift 3
case "$row" in
  run) trips=1000; estimate=13619 ;;
  runStatic) trips=1000; estimate=19027 ;;
  counterInt) trips=1000; estimate=6433 ;;
  counterInt-osr) trips=1000000; estimate=6250164 ;;
  twoCells) trips=1000; estimate=8705 ;;
  twoCells-osr) trips=1000000; estimate=8500178 ;;
  *) echo "unknown row: $row" >&2; exit 2 ;;
esac
calls=$((1500000000 / estimate))
root=$(git rev-parse --show-toplevel)
dir=${DIR:-$(mktemp -d /tmp/jr21-icount.XXXXXX)}
mkdir -p "$dir/classes"
/usr/lib/jvm/java-25-openjdk-amd64/bin/javac -d "$dir/classes" "$root/tools/probes/jit/j27/J27PromotedLoopCountQ.java" "$root/tools/probes/jit/r20/R20PLoopStaticBarrier.java" "$root/tools/probes/jit/r20/R20PLoopStaticCells.java"
mkfifo "$dir/go"
exec 9<>"$dir/go"
if [ "$kind" = hs ]; then
  /usr/lib/jvm/java-25-openjdk-amd64/bin/java -cp "$dir/classes" J27PromotedLoopCountQ "$row" "$trips" "$dir/go" "$calls" >"$dir/out" 2>"$dir/err" &
else
  bin=${BIN:?set BIN to the task CratonVM binary}
  env "$@" "$bin" "--$mode" -cp "$dir/classes" J27PromotedLoopCountQ "$row" "$trips" "$dir/go" "$calls" >"$dir/out" 2>"$dir/err" &
fi
vm_pid=$!
for _ in $(seq 1 18000); do
  grep -q '^ready ' "$dir/out" && break
  kill -0 "$vm_pid" 2>/dev/null || { cat "$dir/err" >&2; exit 1; }
  sleep 0.01
done
pid=$(awk '/^ready / {print $2; exit}' "$dir/out")
ps -T -p "$pid" -o tid=,comm= > "$dir/threads"
[ -n "$pid" ] || { echo "workload did not quiesce" >&2; exit 1; }
for window in 1 2; do
  perf stat -x, --no-big-num --per-thread -e instructions:u -p "$pid" -o "$dir/perf.$window" -- \
    bash -c 'printf "go\n" > "$1"; for _ in $(seq 1 20000); do grep -q "^window $2 " "$3" && exit 0; sleep 0.005; done; exit 1' \
    bash "$dir/go" "$window" "$dir/out"
  grep -q "^window $window " "$dir/out"
done
wait "$vm_pid"
pick_main() {
  awk -F, '/^#/ || NF < 4 || $4 !~ /instructions/ {next}
    {gsub(/[[:space:]]/, "", $2); if ($2 !~ /^[0-9]+$/) next
     t=$1; sub(/.*-/, "", t); if ($2 > best) {best=$2; tid=t}}
    END {if (tid != "") print tid, best; else exit 1}' "$1"
}
count_thread() {
  awk -F, -v tid="$2" '/^#/ || NF < 4 || $4 !~ /instructions/ {next}
    {gsub(/[[:space:]]/, "", $2); t=$1; sub(/.*-/, "", t)
     if (t == tid && $2 ~ /^[0-9]+$/) {print $2; found=1}}
    END {if (!found) exit 1}' "$1"
}
read -r tid n_count < <(pick_main "$dir/perf.1")
three_count=$(count_thread "$dir/perf.2" "$tid")
awk -v a="$n_count" -v b="$three_count" -v n="$calls" -v row="$row" -v kind="$kind" -v tid="$tid" 'BEGIN {printf "%s %s tid=%s calls=%d instructions/call=%.1f\n", kind, row, tid, n, (b-a)/(2*n)}'
cat "$dir/out"
