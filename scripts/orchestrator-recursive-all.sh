#!/usr/bin/env bash
# Recursive runner — walks apps/ recursively, finds every JAR/WAR with a
# Main-Class manifest entry, runs it under CratonVM with a hard timeout,
# captures rc + first error line + elapsed wall time per run, and emits a
# CSV plus a sorted summary at the end.
#
# Usage:
#   bash scripts/orchestrator-recursive-all.sh                 # all jars under apps/
#   bash scripts/orchestrator-recursive-all.sh -t 60           # timeout each run at 60s (default 30)
#   bash scripts/orchestrator-recursive-all.sh -d apps/payara6 # restrict to one subtree
#   bash scripts/orchestrator-recursive-all.sh -s "version --help -h"  # invocation args after Main
#   bash scripts/orchestrator-recursive-all.sh -x 1000         # skip jars bigger than 1000 MB
#   bash scripts/orchestrator-recursive-all.sh -m 200          # limit to first N runnable jars
#
# Notes:
#   * Only jars whose MANIFEST.MF declares Main-Class are run — library jars
#     without one are skipped (and counted in the summary).
#   * Each jar runs in isolation: the classpath is JUST that jar (the
#     manifest may extend with Class-Path:).
#   * Timeout uses `timeout --foreground -k 5 N` (SIGTERM then SIGKILL after
#     5s). Exit code 124 = timeout.
#   * Results go to applogs/orchestrator-recursive-$STAMP/ (one .out, .err,
#     .rc per jar) + results.csv + summary.txt.

set +e

ROOT="C:/Projects/CratonVM"
RJVM="$ROOT/target/release/cratonvm.exe"
JDK="C:/Program Files/Eclipse Adoptium/jdk-25.0.2.10-hotspot"

TIMEOUT_S=30
SCAN_DIR="$ROOT/apps"
INVOCATION_ARGS=""        # if empty, run jar with no args; some apps need
                          # `--help` to do anything other than wait
MAX_SIZE_MB=200
MAX_RUNS=0                # 0 = unlimited

while getopts ":t:d:s:x:m:h" opt; do
    case "$opt" in
        t) TIMEOUT_S="$OPTARG" ;;
        d) SCAN_DIR="$OPTARG" ;;
        s) INVOCATION_ARGS="$OPTARG" ;;
        x) MAX_SIZE_MB="$OPTARG" ;;
        m) MAX_RUNS="$OPTARG" ;;
        h)
            sed -n '4,/^set +e/p' "$0" | sed '$d'
            exit 0
            ;;
    esac
done

STAMP="$(date +%Y%m%d-%H%M%S)"
LOGDIR="$ROOT/applogs/orchestrator-recursive-$STAMP"
mkdir -p "$LOGDIR"
CSV="$LOGDIR/results.csv"
SUMMARY="$LOGDIR/summary.txt"
RUN_LOG="$LOGDIR/run.log"

# CSV header
echo "rel_path,size_mb,main_class,rc,elapsed_s,classification,first_error" > "$CSV"

# Helpers --------------------------------------------------------------------

# read_main_class JAR_PATH — print Main-Class line content, or empty string.
#
# Cygwin pipe-size limits make `unzip -p HUGE.jar MANIFEST.MF | head` blow up
# with "File too large" / "child_copy failed (win32 error 299)" on jars with
# unusual manifests. We bound bytes EARLY with `head -c` (not -n — line
# splitting still runs the full input through awk), and we skip jars whose
# MANIFEST.MF doesn't exist quickly via `unzip -l` first.
read_main_class() {
    local jar="$1"
    # Quick existence check — empty stdout if MANIFEST.MF absent.
    unzip -l "$jar" META-INF/MANIFEST.MF 2>/dev/null \
        | grep -q 'META-INF/MANIFEST.MF' || return 0
    # Now extract, bounded to 8 KB.
    unzip -p "$jar" META-INF/MANIFEST.MF 2>/dev/null \
        | head -c 8192 \
        | tr -d '\r' \
        | awk '/^Main-Class:/ {print substr($0, 13); exit}' \
        | sed 's/^ *//; s/ *$//'
}

# size_mb FILE — file size in MB (rounded down).
size_mb() {
    local f="$1"
    local bytes
    bytes=$(stat -c '%s' "$f" 2>/dev/null) || bytes=0
    echo $((bytes / 1024 / 1024))
}

# classify_run RC FIRST_ERR — one-word classification.
classify_run() {
    local rc="$1"
    local err="$2"
    case "$rc" in
        0)   echo "pass" ;;
        124) echo "timeout" ;;
        *)
            if echo "$err" | grep -qiE 'NoClassDefFound|NoSuchMethod|NoSuchField|AbstractMethod|VerifyError|UnsupportedClassVersion|ClassFormatError'; then
                echo "linkage"
            elif echo "$err" | grep -qiE 'NullPointer|IllegalState|Cannot invoke|Cannot read'; then
                echo "npe"
            elif echo "$err" | grep -qiE 'StackOverflow|OutOfMemory'; then
                echo "resource"
            elif echo "$err" | grep -qiE 'undersized object layout|stale pointer|ARRAY-LEN-GUARD'; then
                echo "vm-bug"
            elif echo "$err" | grep -qiE 'silent-swallow|B6:'; then
                echo "swallowed"
            elif echo "$err" | grep -qiE 'Exception|Error|FAIL'; then
                echo "app-error"
            else
                echo "other"
            fi
            ;;
    esac
}

# run_one JAR_REL_PATH — execute one jar; append to CSV.
run_one() {
    local rel="$1"
    local jar="$SCAN_DIR/$rel"

    local mb
    mb=$(size_mb "$jar")
    if [ "$mb" -gt "$MAX_SIZE_MB" ]; then
        echo "$rel,$mb,SKIP,oversize,0,skip-too-big," >> "$CSV"
        return
    fi

    local main
    main=$(read_main_class "$jar")
    if [ -z "$main" ]; then
        echo "$rel,$mb,,nomain,0,skip-no-main," >> "$CSV"
        return
    fi

    # Replace / and \ in rel with __ so we can use it as a filename
    local safe
    safe=$(echo "$rel" | tr '/\\' '__')
    local out="$LOGDIR/$safe.out"
    local err="$LOGDIR/$safe.err"

    local t0 t1
    t0=$(date +%s)

    # `timeout --foreground` propagates SIGINT/SIGTERM from the orchestrator
    # to the jar process. `-k 5` sends SIGKILL 5s after SIGTERM if it
    # didn't die. `eval` so INVOCATION_ARGS gets tokenised, not quoted.
    if [ -z "$INVOCATION_ARGS" ]; then
        timeout --foreground -k 5 "$TIMEOUT_S" \
            "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx 512m \
            --jar "$jar" \
            > "$out" 2> "$err"
    else
        # shellcheck disable=SC2086
        timeout --foreground -k 5 "$TIMEOUT_S" \
            "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx 512m \
            --jar "$jar" -- $INVOCATION_ARGS \
            > "$out" 2> "$err"
    fi
    local rc=$?
    t1=$(date +%s)
    local elapsed=$((t1 - t0))

    # First "interesting" stderr line — skip our own noise.
    local first_err
    first_err=$(grep -vE '^\[2m.*WARN.*Post-clinit|^\[2m.*WARN.*B6:|^\[2m.*WARN.*Stale|^\[2m.*WARN.*Missing native|^\[2m.*WARN.*SWALLOW|^\[DBG\]|^\[cratonvm\]|^\[rustjvm\]|^\[2m.*WARN.*registered|short-circuited|^\[kc17-bf\]|^\[jboss-bf\]' "$err" 2>/dev/null \
        | grep -E 'Exception|Error|Caused|StringIndex|NullPointer|StackOverflow|ARRAY-LEN-GUARD|gen_heap|FileNot|UnsupportedClass|AbstractMethod|UnknownModule|^Failed|severe|SEVERE|FATAL|fatal|silent-swallow' \
        | head -1 | head -c 220 | sed 's/\x1b\[[0-9;]*m//g')

    local cls
    cls=$(classify_run "$rc" "$first_err")

    # CSV escaping — wrap fields that may contain commas / quotes.
    local err_csv
    err_csv=$(echo "$first_err" | tr -d '"' | tr ',' ' ' | head -c 200)
    echo "$rel,$mb,$main,$rc,$elapsed,$cls,$err_csv" >> "$CSV"

    # Tee a one-liner to run log + stdout so the user sees progress live.
    local line
    line=$(printf "%-60.60s | rc=%-3s | %6ds | %-9s | %s" "$rel" "$rc" "$elapsed" "$cls" "$first_err")
    echo "$line"
    echo "$line" >> "$RUN_LOG"
}

# Discovery -------------------------------------------------------------------

echo "Scanning $SCAN_DIR for JARs/WARs (timeout=${TIMEOUT_S}s, max-size=${MAX_SIZE_MB}MB)..." | tee "$RUN_LOG"
mapfile -t ALL_JARS < <(cd "$SCAN_DIR" && find . -type f \( -name '*.jar' -o -name '*.war' \) | sed 's|^\./||' | sort)
echo "Found ${#ALL_JARS[@]} jar/war files under $SCAN_DIR." | tee -a "$RUN_LOG"

# Filter to jars with Main-Class so we don't waste cycles on libs.
RUNNABLE=()
for rel in "${ALL_JARS[@]}"; do
    main=$(read_main_class "$SCAN_DIR/$rel")
    [ -n "$main" ] && RUNNABLE+=("$rel")
done
echo "${#RUNNABLE[@]} of those declare Main-Class and will be run." | tee -a "$RUN_LOG"

# Apply MAX_RUNS limit (testing/debug).
if [ "$MAX_RUNS" -gt 0 ] && [ "${#RUNNABLE[@]}" -gt "$MAX_RUNS" ]; then
    RUNNABLE=("${RUNNABLE[@]:0:$MAX_RUNS}")
    echo "Limited to first $MAX_RUNS runs." | tee -a "$RUN_LOG"
fi

# Run -------------------------------------------------------------------------

T_START=$(date +%s)
COUNT=0
for rel in "${RUNNABLE[@]}"; do
    COUNT=$((COUNT + 1))
    printf "[%3d/%3d] " "$COUNT" "${#RUNNABLE[@]}" >&2
    run_one "$rel"
done
T_END=$(date +%s)
TOTAL_S=$((T_END - T_START))

# Summary --------------------------------------------------------------------

{
    echo ""
    echo "=================== Summary ==================="
    echo "Scan dir:       $SCAN_DIR"
    echo "Timeout/run:    ${TIMEOUT_S}s"
    echo "Total jars:     ${#ALL_JARS[@]}"
    echo "Runnable:       ${#RUNNABLE[@]}"
    echo "Total wall:     ${TOTAL_S}s"
    echo ""
    echo "-- by classification --"
    tail -n +2 "$CSV" | awk -F, '{print $6}' | sort | uniq -c | sort -rn
    echo ""
    echo "-- by rc --"
    tail -n +2 "$CSV" | awk -F, '{print $4}' | sort | uniq -c | sort -rn
    echo ""
    echo "-- top 10 slowest runs --"
    tail -n +2 "$CSV" | sort -t, -k5 -rn | head -10 | awk -F, '{printf "  %5ss  rc=%s  %s\n", $5, $4, $1}'
    echo ""
    echo "-- first 20 distinct first-error lines --"
    tail -n +2 "$CSV" | awk -F, '$7 != "" {print $7}' | sort -u | head -20
    echo ""
    echo "Full CSV: $CSV"
    echo "Per-jar logs: $LOGDIR/*.{out,err}"
} | tee -a "$SUMMARY"
