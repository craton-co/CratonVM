#!/usr/bin/env bash
# bench/wave2-3/run-under-cratonvm.sh
# WP2.3-D — execute the staged CGLIB and ByteBuddy probes under
# cratonvm and capture stdout/stderr/rc deterministically. Mirrors
# bench/wildfly/run-under-cratonvm.sh.
#
# Pre-conditions: stage-cglib-probe.sh AND stage-bytebuddy-probe.sh
# have already been run. Each may have written a 'skipped.flag' if
# the relevant jar wasn't found — the run still proceeds with the
# fallback synthetic probe so the harness exits cleanly.
#
# Usage:
#   bash bench/wave2-3/run-under-cratonvm.sh
# Env:
#   CRATONVM_BIN  override path to cratonvm[.exe].
#   TIMEOUT_SEC  per-probe seconds before kill (default 60).
#   PROBE        run only this probe ("cglib" | "bytebuddy"). default both.
#
# Artifacts (per-probe under bench/wave2-3/):
#   last-run-<probe>.stdout.log
#   last-run-<probe>.stderr.log
#   last-run-<probe>.rc
#   last-run-<probe>.meta.json
#   summary.json    (overall pass/fail/skip rollup)
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/../.." && pwd)"

# Resolve cratonvm binary.
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

# Convert /c/foo/bar to C:\foo\bar so the Windows-built cratonvm.exe can read
# its own classpath. cygpath handles this when available; otherwise a tiny
# regex does the job.
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

    : > "$stdout_log"
    : > "$stderr_log"

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
  "rc":           127,
  "status":       "not-staged",
  "timeout_sec":  $TIMEOUT_SEC
}
JSON
        return 0
    fi

    local main_class
    main_class="$(head -n 1 "$main_file" | tr -d '[:space:]')"
    if [[ -z "$main_class" ]]; then
        echo "run-under-cratonvm: probe=$probe ERROR main-class.txt empty" >&2
        echo "126" > "$rc_file"
        return 0
    fi

    # Build classpath: classes dir + every staged jar.
    local cp_parts=("$(to_native_path "$classes_dir")")
    shopt -s nullglob
    for jar in "$staged_dir"/*.jar; do
        cp_parts+=("$(to_native_path "$jar")")
    done
    shopt -u nullglob
    local cp=""
    for p in "${cp_parts[@]}"; do
        if [[ -z "$cp" ]]; then cp="$p"; else cp="${cp}${CPSEP}${p}"; fi
    done

    local skipped="no"
    if [[ -f "$skip_flag" ]]; then skipped="yes"; fi

    echo "run-under-cratonvm: probe=$probe binary=$CRATONVM"
    echo "run-under-cratonvm: probe=$probe cp=$cp"
    echo "run-under-cratonvm: probe=$probe main=$main_class skipped-real-jar=$skipped"
    echo "run-under-cratonvm: probe=$probe timeout=${TIMEOUT_SEC}s"

    set +e
    if command -v timeout >/dev/null 2>&1; then
        timeout --kill-after=5 "${TIMEOUT_SEC}" \
            "$CRATONVM" -c "$cp" "$main_class" \
            > "$stdout_log" 2> "$stderr_log"
        local rc=$?
    else
        "$CRATONVM" -c "$cp" "$main_class" \
            > "$stdout_log" 2> "$stderr_log"
        local rc=$?
    fi
    set -e
    echo "$rc" > "$rc_file"

    cat > "$meta_file" <<JSON
{
  "probe":        "$probe",
  "generated_at": "$TS",
  "cratonvm_bin":  "$CRATONVM",
  "cratonvm_rev":  "$GIT_REV",
  "main_class":   "$main_class",
  "classpath":    "$cp",
  "rc":           $rc,
  "status":       "ran",
  "skipped_real_jar": "$skipped",
  "timeout_sec":  $TIMEOUT_SEC
}
JSON
    echo "run-under-cratonvm: probe=$probe rc=$rc"
}

# Decide which probes to run.
PROBE_FILTER="${PROBE:-both}"
case "$PROBE_FILTER" in
    cglib|bytebuddy|both) ;;
    *) echo "run-under-cratonvm: ERROR PROBE must be cglib | bytebuddy | both" >&2; exit 4 ;;
esac

if [[ "$PROBE_FILTER" == "cglib" || "$PROBE_FILTER" == "both" ]]; then
    run_probe cglib
fi
if [[ "$PROBE_FILTER" == "bytebuddy" || "$PROBE_FILTER" == "both" ]]; then
    run_probe bytebuddy
fi

# Roll up summary.
RC_CGLIB="-1"
RC_BB="-1"
[[ -f "$HERE/last-run-cglib.rc" ]] && RC_CGLIB="$(head -n 1 "$HERE/last-run-cglib.rc" | tr -d '[:space:]')"
[[ -f "$HERE/last-run-bytebuddy.rc" ]] && RC_BB="$(head -n 1 "$HERE/last-run-bytebuddy.rc" | tr -d '[:space:]')"
SKIP_CGLIB="no"; [[ -f "$HERE/staged-cglib/skipped.flag" ]] && SKIP_CGLIB="yes"
SKIP_BB="no";    [[ -f "$HERE/staged-bytebuddy/skipped.flag" ]] && SKIP_BB="yes"

cat > "$HERE/summary.json" <<JSON
{
  "generated_at": "$TS",
  "cratonvm_bin":  "$CRATONVM",
  "cratonvm_rev":  "$GIT_REV",
  "probes": {
    "cglib":     {"rc": $RC_CGLIB, "skipped_real_jar": "$SKIP_CGLIB"},
    "bytebuddy": {"rc": $RC_BB,    "skipped_real_jar": "$SKIP_BB"}
  }
}
JSON

echo "run-under-cratonvm: summary -> $HERE/summary.json"
exit 0
