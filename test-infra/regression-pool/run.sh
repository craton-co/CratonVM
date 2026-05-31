#!/usr/bin/env bash
# run.sh — execute every probe in pool.tsv, diff against baselines/, report.
#
# Usage:
#   bash test-infra/regression-pool/run.sh                  # run all
#   bash test-infra/regression-pool/run.sh <name>           # one probe only
#   bash test-infra/regression-pool/run.sh --record <name>  # capture baseline
#   bash test-infra/regression-pool/run.sh --record-all     # capture all baselines
#
# Output:
#   - Per-probe one-liner: name | rc | wall_s | PASS|REGRESS|SLOW|MISSING_BASELINE | summary
#   - results/<ts>.tsv with full rows
#   - Exit 0 if every probe is PASS; 1 if any REGRESS/SLOW; 2 if a probe couldn't
#     be staged (missing app distro).

set +e

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
POOL_DIR="$REPO_ROOT/test-infra/regression-pool"
APPS_ROOT="$POOL_DIR/apps"
BASELINE_DIR="$POOL_DIR/baselines"
RESULTS_DIR="$POOL_DIR/results"
POOL_TSV="$POOL_DIR/pool.tsv"
RJVM="$REPO_ROOT/target/release/cratonvm.exe"
JDK="${JDK:-C:/Program Files/Java/jdk-25}"
XMX="${XMX:-512m}"

mkdir -p "$RESULTS_DIR"
TS=$(date +%Y%m%d-%H%M%S)
OUT_TSV="$RESULTS_DIR/$TS.tsv"

if [ ! -x "$RJVM" ]; then
    echo "ERROR: $RJVM not found. Run 'cargo build --release -p cratonvm-cli' first." >&2
    exit 3
fi

# parse args
MODE="run"
ONLY_NAME=""
for a in "$@"; do
    case "$a" in
        --record)      MODE="record" ;;
        --record-all)  MODE="record-all" ;;
        --help|-h)     sed -n '2,/^set +e/p' "$0" | sed 's/^# \?//'; exit 0 ;;
        --*)           echo "unknown flag $a" >&2; exit 2 ;;
        *)             ONLY_NAME="$a" ;;
    esac
done

# Strip volatile log prefixes from probe stdout (timestamps, GC warnings, etc.)
# so a baseline diff only cares about the probe's actual output.
filter_volatile() {
    sed 's/\x1b\[[0-9;]*m//g' \
      | sed 's/@[0-9a-f]\{1,\}/@<id>/g' \
      | grep -vE '^\[?(2026|2025|2024|2027)-[0-9]+-[0-9]+T[0-9]+:[0-9]+:[0-9]+\.[0-9]+Z' \
      | grep -vE '^\[cratonvm[^]]*\]' \
      | grep -vE 'WARN cratonvm' \
      | grep -vE '^\[DBG\]' \
      | grep -vE 'jar signer:' \
      | grep -vE '^SLF4J:' \
      | grep -vE 'Post-clinit fixup'
}

# Substitute variables in classpath/args
subst() {
    local s="$1"
    s="${s//\$APPS_ROOT/$APPS_ROOT}"
    s="${s//\$REPO_ROOT/$REPO_ROOT}"
    s="${s//\$PROBE_DIR/$PROBE_DIR_ABS}"
    echo "$s"
}

# Expand glob patterns into a real classpath, semicolons preserved
expand_cp() {
    local raw="$1"
    local out=""
    IFS=';' read -ra parts <<<"$raw"
    for p in "${parts[@]}"; do
        if [[ "$p" == *"*"* ]]; then
            # glob expansion
            local matches
            matches=$(ls $p 2>/dev/null | tr '\n' ';')
            out+="$matches"
        else
            out+="$p;"
        fi
    done
    echo "${out%;}"
}

# pool.tsv → iterate
printf "name\trc\twall_s\tstatus\tnote\n" > "$OUT_TSV"
total=0; pass=0; regress=0; slow=0; miss=0; nostage=0

while IFS=$'\t' read -r name app probe_class probe_dir cp_glob args max_seconds baseline_file; do
    [ -z "$name" ] && continue
    [[ "$name" =~ ^# ]] && continue
    [ "$name" = "name" ] && continue  # header row
    # Strip trailing CR — pool.tsv may be checked out with CRLF line endings on
    # Windows, which leaves \r on the last field. Without this, every probe
    # reports NO_BASE because $baseline_file points at "<name>.txt\r".
    baseline_file="${baseline_file%$'\r'}"
    if [ -n "$ONLY_NAME" ] && [ "$name" != "$ONLY_NAME" ]; then
        continue
    fi
    total=$((total+1))

    PROBE_DIR_ABS="$REPO_ROOT/$probe_dir"
    cp_resolved=$(subst "$cp_glob")
    cp_expanded=$(expand_cp "$cp_resolved")
    full_cp="$PROBE_DIR_ABS;$cp_expanded"
    args_resolved=$(subst "$args")
    [ "$args_resolved" = "-" ] && args_resolved=""

    # Sanity-check probe dir + classpath
    if [ ! -d "$PROBE_DIR_ABS" ]; then
        printf "%-25s | rc=-  | %5ss | %-10s | %s\n" "$name" "  -" "MISS_PROBE" "$PROBE_DIR_ABS"
        miss=$((miss+1))
        continue
    fi
    # one of the CP entries doesn't exist on disk?
    # split on ';' first — classpath uses Windows-style separator
    first_missing=""
    IFS=';' read -ra _parts <<<"$cp_resolved"
    for _p in "${_parts[@]}"; do
        [ -z "$_p" ] && continue
        if ! ls $_p >/dev/null 2>&1; then
            first_missing="$_p"
            break
        fi
    done
    if [ -n "$first_missing" ]; then
        printf "%-25s | rc=-  | %5ss | %-10s | %s\n" "$name" "  -" "NOSTAGE" "missing: $first_missing"
        nostage=$((nostage+1))
        continue
    fi

    # Run the probe
    out_file=$(mktemp)
    err_file=$(mktemp)
    t0=$(date +%s%N)
    if [ -n "$args_resolved" ]; then
        timeout --foreground -k 5 "$max_seconds" "$RJVM" \
            --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
            -c "$full_cp" "$probe_class" $args_resolved \
            </dev/null > "$out_file" 2> "$err_file"
    else
        timeout --foreground -k 5 "$max_seconds" "$RJVM" \
            --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
            -c "$full_cp" "$probe_class" \
            </dev/null > "$out_file" 2> "$err_file"
    fi
    rc=$?
    t1=$(date +%s%N)
    wall_ms=$(( (t1 - t0) / 1000000 ))
    wall_s=$(awk -v ms="$wall_ms" 'BEGIN{printf "%.2f", ms/1000}')

    filtered=$(filter_volatile < "$out_file")

    if [ "$MODE" = "record-all" ] || ([ "$MODE" = "record" ] && [ -n "$ONLY_NAME" ]); then
        # baseline capture
        if [ "$rc" -eq 0 ]; then
            echo "$filtered" > "$BASELINE_DIR/$baseline_file"
            printf "%-25s | rc=%-3s | %5ss | %-10s | recorded → %s\n" "$name" "$rc" "$wall_s" "BASELINE" "$baseline_file"
            pass=$((pass+1))
        else
            printf "%-25s | rc=%-3s | %5ss | %-10s | refused to record (probe failed)\n" "$name" "$rc" "$wall_s" "REFUSE"
            regress=$((regress+1))
        fi
    else
        # comparison
        if [ "$rc" -ne 0 ]; then
            first_err=$(grep -E 'Exception|Error|FAIL|Caused' "$err_file" 2>/dev/null | head -1 | sed 's/\x1b\[[0-9;]*m//g' | head -c 120)
            printf "%-25s | rc=%-3s | %5ss | %-10s | %s\n" "$name" "$rc" "$wall_s" "REGRESS" "$first_err"
            regress=$((regress+1))
            printf "%s\t%s\t%s\tREGRESS\t%s\n" "$name" "$rc" "$wall_s" "$first_err" >> "$OUT_TSV"
        elif [ ! -f "$BASELINE_DIR/$baseline_file" ]; then
            printf "%-25s | rc=%-3s | %5ss | %-10s | no baseline (run --record %s)\n" "$name" "$rc" "$wall_s" "NO_BASE" "$name"
            miss=$((miss+1))
            printf "%s\t%s\t%s\tNO_BASE\tno baseline\n" "$name" "$rc" "$wall_s" >> "$OUT_TSV"
        elif ! diff -q <(echo "$filtered") "$BASELINE_DIR/$baseline_file" >/dev/null 2>&1; then
            # diff for context
            d=$(diff <(echo "$filtered") "$BASELINE_DIR/$baseline_file" | head -3 | tr '\n' ' ' | head -c 120)
            printf "%-25s | rc=%-3s | %5ss | %-10s | %s\n" "$name" "$rc" "$wall_s" "REGRESS" "$d"
            regress=$((regress+1))
            printf "%s\t%s\t%s\tREGRESS\t%s\n" "$name" "$rc" "$wall_s" "$d" >> "$OUT_TSV"
        elif [ "$(awk -v w="$wall_s" -v m="$max_seconds" 'BEGIN{print (w > m*0.8) ? 1 : 0}')" = "1" ]; then
            printf "%-25s | rc=%-3s | %5ss | %-10s | wall %s > 80%% of %ss envelope\n" "$name" "$rc" "$wall_s" "SLOW" "$wall_s" "$max_seconds"
            slow=$((slow+1))
            printf "%s\t%s\t%s\tSLOW\t> 80%% envelope\n" "$name" "$rc" "$wall_s" >> "$OUT_TSV"
        else
            printf "%-25s | rc=%-3s | %5ss | %-10s |\n" "$name" "$rc" "$wall_s" "PASS"
            pass=$((pass+1))
            printf "%s\t%s\t%s\tPASS\t\n" "$name" "$rc" "$wall_s" >> "$OUT_TSV"
        fi
    fi

    rm -f "$out_file" "$err_file"
done < "$POOL_TSV"

echo
echo "regression-pool: total=$total pass=$pass regress=$regress slow=$slow no_base=$miss no_stage=$nostage"
echo "results: $OUT_TSV"

if [ "$regress" -gt 0 ]; then
    exit 1
elif [ "$slow" -gt 0 ]; then
    exit 1
elif [ "$nostage" -gt 0 ]; then
    exit 2
else
    exit 0
fi
