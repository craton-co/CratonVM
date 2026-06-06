#!/usr/bin/env bash
# bench.sh — wall-clock + peak RSS per probe over N iterations.
# Use to measure JIT / GC / intrinsic regressions against a stable workload.
#
# Usage:
#   bash test-infra/regression-pool/bench.sh [iterations] [probe-name]
#     iterations default 5
#     probe-name optional, default = run every row in pool.tsv
#
# Output:
#   results/<ts>.bench.tsv with columns:
#     name | iter | rc | wall_ms | rss_kb_peak
#   Plus a summary table on stdout:
#     name | n | wall_ms (median / p95) | rss_kb (median / max)

set +e

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
POOL_DIR="$REPO_ROOT/test-infra/regression-pool"
APPS_ROOT="$POOL_DIR/apps"
POOL_TSV="$POOL_DIR/pool.tsv"
RESULTS_DIR="$POOL_DIR/results"
RJVM="$REPO_ROOT/target/release/cratonvm.exe"
JDK="${JDK:-C:/Program Files/Java/jdk-25}"
XMX="${XMX:-512m}"

mkdir -p "$RESULTS_DIR"
TS=$(date +%Y%m%d-%H%M%S)
OUT="$RESULTS_DIR/$TS.bench.tsv"
printf "name\titer\trc\twall_ms\trss_kb_peak\n" > "$OUT"

ITERS="${1:-5}"
ONLY="${2:-}"

subst() {
    local s="$1"
    s="${s//\$APPS_ROOT/$APPS_ROOT}"
    s="${s//\$REPO_ROOT/$REPO_ROOT}"
    s="${s//\$PROBE_DIR/$PROBE_DIR_ABS}"
    echo "$s"
}
expand_cp() {
    local raw="$1" out=""
    IFS=';' read -ra parts <<<"$raw"
    for p in "${parts[@]}"; do
        if [[ "$p" == *"*"* ]]; then
            out+="$(ls $p 2>/dev/null | tr '\n' ';')"
        else
            out+="$p;"
        fi
    done
    echo "${out%;}"
}

# Peak RSS for a child process via tasklist (Windows) — sampled mid-run.
# We background the probe, poll tasklist every 200 ms, take max.
peak_rss_kb() {
    local pid="$1"
    local peak=0
    while kill -0 "$pid" 2>/dev/null; do
        local cur
        cur=$(tasklist //FI "PID eq $pid" //FO CSV //NH 2>/dev/null \
            | head -1 | awk -F'","' '{gsub(/[" ,K]/, "", $NF); print $NF}')
        if [[ "$cur" =~ ^[0-9]+$ ]] && (( cur > peak )); then
            peak=$cur
        fi
        sleep 0.2
    done
    echo "$peak"
}

declare -A WALL_BY_NAME RSS_BY_NAME COUNT_BY_NAME

while IFS=$'\t' read -r name app probe_class probe_dir cp_glob args max_seconds baseline_file; do
    [ -z "$name" ] && continue
    [[ "$name" =~ ^# ]] && continue
    [ "$name" = "name" ] && continue  # header row
    baseline_file="${baseline_file%$'\r'}"  # strip CR from CRLF-mode checkout
    [ -n "$ONLY" ] && [ "$name" != "$ONLY" ] && continue

    PROBE_DIR_ABS="$REPO_ROOT/$probe_dir"
    cp_resolved=$(subst "$cp_glob")
    cp_expanded=$(expand_cp "$cp_resolved")
    full_cp="$PROBE_DIR_ABS;$cp_expanded"
    args_resolved=$(subst "$args")
    [ "$args_resolved" = "-" ] && args_resolved=""

    [ ! -d "$PROBE_DIR_ABS" ] && { echo "[$name] skipping — no probe dir"; continue; }
    # Split classpath on ';' (Windows-style); fail if any part is missing.
    cp_ok=1
    IFS=';' read -ra _parts <<<"$cp_resolved"
    for _p in "${_parts[@]}"; do
        [ -z "$_p" ] && continue
        if ! ls $_p >/dev/null 2>&1; then
            echo "[$name] skipping — classpath entry not staged: $_p"
            cp_ok=0
            break
        fi
    done
    [ "$cp_ok" = "0" ] && continue

    echo "=== $name × $ITERS ==="
    walls=()
    rsses=()
    for ((i=1; i<=ITERS; i++)); do
        t0=$(date +%s%N)
        if [ -n "$args_resolved" ]; then
            "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
                -c "$full_cp" "$probe_class" $args_resolved \
                </dev/null >/dev/null 2>&1 &
        else
            "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
                -c "$full_cp" "$probe_class" \
                </dev/null >/dev/null 2>&1 &
        fi
        pid=$!
        rss=$(peak_rss_kb "$pid")
        wait "$pid"
        rc=$?
        t1=$(date +%s%N)
        wall=$(( (t1 - t0) / 1000000 ))
        echo "  iter $i: rc=$rc wall=${wall}ms rss=${rss}kb"
        printf "%s\t%d\t%d\t%d\t%d\n" "$name" "$i" "$rc" "$wall" "$rss" >> "$OUT"
        walls+=("$wall")
        rsses+=("$rss")
    done

    # median + p95
    walls_sorted=$(printf "%s\n" "${walls[@]}" | sort -n)
    rsses_sorted=$(printf "%s\n" "${rsses[@]}" | sort -n)
    median_wall=$(echo "$walls_sorted" | awk -v n=$ITERS 'NR==int((n+1)/2)')
    p95_wall=$(echo "$walls_sorted" | awk -v n=$ITERS 'NR==int(n*0.95+0.5)')
    median_rss=$(echo "$rsses_sorted" | awk -v n=$ITERS 'NR==int((n+1)/2)')
    max_rss=$(echo "$rsses_sorted" | tail -1)
    printf "  median wall=%sms p95=%sms | median rss=%skb max=%skb\n" \
        "$median_wall" "$p95_wall" "$median_rss" "$max_rss"

    WALL_BY_NAME[$name]="$median_wall/$p95_wall"
    RSS_BY_NAME[$name]="$median_rss/$max_rss"
done < "$POOL_TSV"

echo
printf "%-25s | %-15s | %-15s\n" "name" "wall_ms(med/p95)" "rss_kb(med/max)"
echo "-----------------------+-----------------+-----------------"
for n in "${!WALL_BY_NAME[@]}"; do
    printf "%-25s | %-15s | %-15s\n" "$n" "${WALL_BY_NAME[$n]}" "${RSS_BY_NAME[$n]}"
done | sort

echo
echo "bench results: $OUT"
