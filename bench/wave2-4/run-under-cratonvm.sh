#!/usr/bin/env bash
# bench/wave2-4/run-under-cratonvm.sh
# WP2.4-D — execute the staged instrument / jacoco / mockito probes
# under cratonvm and capture stdout/stderr/rc deterministically.
#
# Pre-conditions: stage-{instrument,jacoco,mockito}-probe.sh have all
# already run. Each may have written staged-*/skipped.flag if a
# required jar wasn't found — the run still proceeds, but skipped
# probes are recorded as rc=-2 in summary.json (so drift detection
# can distinguish skip from failure).
#
# Env:
#   CRATONVM_BIN   override path to cratonvm[.exe].
#   TIMEOUT_SEC   per-probe timeout (default 60).
#   PROBE         "instrument" | "jacoco" | "mockito" | "all" (default all).
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/../.." && pwd)"

if [[ -n "${CRATONVM_BIN:-}" && -x "$CRATONVM_BIN" ]]; then
    CRATONVM="$CRATONVM_BIN"
elif [[ -x "$REPO_ROOT/target/release/cratonvm.exe" ]]; then
    CRATONVM="$REPO_ROOT/target/release/cratonvm.exe"
elif [[ -x "$REPO_ROOT/target/release/cratonvm" ]]; then
    CRATONVM="$REPO_ROOT/target/release/cratonvm"
else
    echo "run-under-cratonvm: ERROR cratonvm binary not found; build with 'cargo build --release -p cratonvm-cli'" >&2
    exit 3
fi

case "$(uname -s 2>/dev/null || echo Windows)" in
    MINGW*|MSYS*|CYGWIN*|Windows*) CPSEP=';'; IS_WINDOWS=1 ;;
    *) CPSEP=':'; IS_WINDOWS=0 ;;
esac

to_native_path() {
    local p="$1"
    if [[ "$IS_WINDOWS" == "1" ]]; then
        if command -v cygpath >/dev/null 2>&1; then
            cygpath -w "$p"
        elif [[ "$p" =~ ^/([a-zA-Z])/(.*)$ ]]; then
            local drive="${BASH_REMATCH[1]}"
            local rest="${BASH_REMATCH[2]}"
            echo "${drive^^}:\\${rest//\//\\}"
        else
            echo "$p"
        fi
    else
        echo "$p"
    fi
}

TIMEOUT_SEC="${TIMEOUT_SEC:-60}"
TS="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

GIT_REV="unknown"
if command -v git >/dev/null 2>&1 && git -C "$REPO_ROOT" rev-parse HEAD >/dev/null 2>&1; then
    GIT_REV="$(git -C "$REPO_ROOT" rev-parse --short HEAD)"
fi

# ---------------------------------------------------------------------------
# Per-probe runner. Each probe declares:
#   - its staged dir
#   - which jars (if any) join the classpath
#   - the -javaagent: spec to pass to cratonvm
# ---------------------------------------------------------------------------
run_probe() {
    local probe="$1"
    local staged_dir="$HERE/staged-$probe"
    local classes_dir="$staged_dir/classes"
    local main_file="$staged_dir/main-class.txt"
    local skip_flag="$staged_dir/skipped.flag"
    local stdout_log="$HERE/last-run-$probe.stdout.log"
    local stderr_log="$HERE/last-run-$probe.stderr.log"
    local rc_file="$HERE/last-run-$probe.rc"
    local meta_file="$HERE/last-run-$probe.meta.json"
    local jacoco_exec="$HERE/last-run-$probe.jacoco.exec"

    : > "$stdout_log"
    : > "$stderr_log"
    rm -f "$jacoco_exec"

    if [[ ! -d "$classes_dir" ]] || [[ ! -f "$main_file" ]]; then
        echo "run-under-cratonvm: probe=$probe NOT-STAGED (run stage-$probe-probe.sh first)" >&2
        echo "127" > "$rc_file"
        cat > "$meta_file" <<JSON
{
  "probe":        "$probe",
  "generated_at": "$TS",
  "cratonvm_bin":  "$CRATONVM",
  "cratonvm_rev":  "$GIT_REV",
  "main_class":   "",
  "classpath":    "",
  "javaagent":    "",
  "rc":           127,
  "status":       "not-staged",
  "skipped_real_jar": "no",
  "timeout_sec":  $TIMEOUT_SEC
}
JSON
        return 0
    fi

    local main_class
    main_class="$(head -n 1 "$main_file" | tr -d '[:space:]')"

    local skipped="no"
    if [[ -f "$skip_flag" ]]; then skipped="yes"; fi

    # Skipped probes: no run, no cratonvm invocation. summary.json records
    # rc=-2 so drift tools can tell skip from failure.
    if [[ "$skipped" == "yes" ]]; then
        echo "run-under-cratonvm: probe=$probe SKIPPED (jar absent at stage time)"
        echo "-2" > "$rc_file"
        cat > "$meta_file" <<JSON
{
  "probe":        "$probe",
  "generated_at": "$TS",
  "cratonvm_bin":  "$CRATONVM",
  "cratonvm_rev":  "$GIT_REV",
  "main_class":   "$main_class",
  "classpath":    "",
  "javaagent":    "",
  "rc":           -2,
  "status":       "skipped",
  "skipped_real_jar": "yes",
  "timeout_sec":  $TIMEOUT_SEC
}
JSON
        return 0
    fi

    # Build classpath + javaagent per-probe.
    local cp_parts=()
    local javaagent_arg=""
    local jacoco_destfile=""
    case "$probe" in
        instrument)
            # cp = staged classes only. agent jar is passed via -javaagent:.
            cp_parts=("$(to_native_path "$classes_dir")")
            javaagent_arg="-javaagent:$(to_native_path "$staged_dir/agent.jar")"
            ;;
        jacoco)
            cp_parts=("$(to_native_path "$classes_dir")")
            jacoco_destfile="$(to_native_path "$jacoco_exec")"
            javaagent_arg="-javaagent:$(to_native_path "$staged_dir/jacocoagent.jar")=destfile=$jacoco_destfile"
            ;;
        mockito)
            cp_parts=("$(to_native_path "$classes_dir")")
            for jar in mockito-core.jar byte-buddy.jar byte-buddy-agent.jar objenesis.jar; do
                if [[ -f "$staged_dir/$jar" ]]; then
                    cp_parts+=("$(to_native_path "$staged_dir/$jar")")
                fi
            done
            javaagent_arg="-javaagent:$(to_native_path "$staged_dir/byte-buddy-agent.jar")"
            ;;
    esac

    local cp=""
    for p in "${cp_parts[@]}"; do
        if [[ -z "$cp" ]]; then cp="$p"; else cp="${cp}${CPSEP}${p}"; fi
    done

    echo "run-under-cratonvm: probe=$probe binary=$CRATONVM"
    echo "run-under-cratonvm: probe=$probe javaagent=$javaagent_arg"
    echo "run-under-cratonvm: probe=$probe cp=$cp"
    echo "run-under-cratonvm: probe=$probe main=$main_class"
    echo "run-under-cratonvm: probe=$probe timeout=${TIMEOUT_SEC}s"

    # Argv layout: cratonvm CLI takes -javaagent: BEFORE -c so it's
    # captured by the hotspot-flag pass; -c <cp> <main> follows.
    set +e
    if command -v timeout >/dev/null 2>&1; then
        timeout --kill-after=5 "${TIMEOUT_SEC}" \
            "$CRATONVM" "$javaagent_arg" -c "$cp" "$main_class" \
            > "$stdout_log" 2> "$stderr_log"
        local rc=$?
    else
        "$CRATONVM" "$javaagent_arg" -c "$cp" "$main_class" \
            > "$stdout_log" 2> "$stderr_log"
        local rc=$?
    fi
    set -e
    echo "$rc" > "$rc_file"

    # Probe-specific post-run sanity checks (purely informational —
    # do NOT change rc).
    local jacoco_present="false"
    local jacoco_size=0
    local jacoco_magic_ok="false"
    if [[ "$probe" == "jacoco" ]]; then
        if [[ -f "$jacoco_exec" ]]; then
            jacoco_present="true"
            jacoco_size=$(wc -c < "$jacoco_exec" 2>/dev/null || echo 0)
            # JaCoCo execdata starts with the bytes 0x01 0xC0 0xC0 (block
            # header + magic). We accept any non-empty file as soft-pass
            # and additionally flag whether the magic is present.
            if [[ "$jacoco_size" -gt 3 ]]; then
                head_bytes="$(head -c 3 "$jacoco_exec" | od -An -tx1 | tr -d ' \n')"
                if [[ "$head_bytes" == "01c0c0" ]]; then
                    jacoco_magic_ok="true"
                fi
            fi
        fi
    fi

    cat > "$meta_file" <<JSON
{
  "probe":        "$probe",
  "generated_at": "$TS",
  "cratonvm_bin":  "$CRATONVM",
  "cratonvm_rev":  "$GIT_REV",
  "main_class":   "$main_class",
  "classpath":    "$cp",
  "javaagent":    "$javaagent_arg",
  "rc":           $rc,
  "status":       "ran",
  "skipped_real_jar": "$skipped",
  "timeout_sec":  $TIMEOUT_SEC,
  "jacoco_exec_present": $jacoco_present,
  "jacoco_exec_size":    $jacoco_size,
  "jacoco_magic_ok":     $jacoco_magic_ok
}
JSON
    echo "run-under-cratonvm: probe=$probe rc=$rc"
}

PROBE_FILTER="${PROBE:-all}"
case "$PROBE_FILTER" in
    instrument|jacoco|mockito|all) ;;
    *) echo "run-under-cratonvm: ERROR PROBE must be instrument | jacoco | mockito | all" >&2; exit 4 ;;
esac

run_one() {
    local p="$1"
    if [[ "$PROBE_FILTER" == "all" || "$PROBE_FILTER" == "$p" ]]; then
        run_probe "$p"
    fi
}
run_one instrument
run_one jacoco
run_one mockito

# Roll up summary.
read_rc()  { local f="$1"; [[ -f "$f" ]] && head -n 1 "$f" | tr -d '[:space:]' || echo "-1"; }
skip_yes() { local d="$1"; [[ -f "$d/skipped.flag" ]] && echo "yes" || echo "no"; }

RC_INS="$(read_rc "$HERE/last-run-instrument.rc")"
RC_JCC="$(read_rc "$HERE/last-run-jacoco.rc")"
RC_MCK="$(read_rc "$HERE/last-run-mockito.rc")"
SK_INS="$(skip_yes "$HERE/staged-instrument")"
SK_JCC="$(skip_yes "$HERE/staged-jacoco")"
SK_MCK="$(skip_yes "$HERE/staged-mockito")"

cat > "$HERE/summary.json" <<JSON
{
  "generated_at": "$TS",
  "cratonvm_bin":  "$CRATONVM",
  "cratonvm_rev":  "$GIT_REV",
  "probes": {
    "instrument": {"rc": $RC_INS, "skipped_real_jar": "$SK_INS"},
    "jacoco":     {"rc": $RC_JCC, "skipped_real_jar": "$SK_JCC"},
    "mockito":    {"rc": $RC_MCK, "skipped_real_jar": "$SK_MCK"}
  }
}
JSON

echo "run-under-cratonvm: summary -> $HERE/summary.json"
exit 0
