#!/bin/bash
# CratonBench reliability gate — decides whether a benchmark run is allowed to
# count as evidence at all.
#
# The perf gate (run-cratonbench-gate.sh) answers "did the median regress?".
# That question is only meaningful when the measurement itself is sound, and
# this repository has already published numbers that were not: BENCHMARK.md
# carries a RETRACTED HashMap regression (22,077 ms / 21.2x) that does not
# reproduce, and a String/Regex row from the same session that has never been
# re-measured. Both passed every check the perf gate had, because the perf gate
# had no check that could see them. This script is that missing check.
#
# It refuses a run — non-zero exit, explicit reason string — when any of:
#
#   * a checksum differs between runs of a phase, or differs from the
#     reference checksum recorded in the baseline file;
#   * a placeholder / zero / missing baseline would be used as a regression
#     threshold (a zero baseline silently makes every budget zero, so the gate
#     either always fails or, with a `>=` comparison, always passes);
#   * fewer samples than the protocol requires were collected;
#   * host load exceeded the ceiling at the start of the run OR at any point
#     during it (per-sample load is recorded, not just the opening reading);
#   * the measured process migrated between CPUs, the pin was not a single
#     CPU, the core throttled thermally, the core's frequency moved more than
#     the allowed spread, or run-to-run variance (CV) exceeded the ceiling;
#   * the environment manifest is missing or incomplete (revision, binary
#     hash, flags, JDK version, CPU model, command line, host load).
#
# Usage:
#   reliability-gate.sh preflight     --results DIR [options]
#   reliability-gate.sh postflight    --results DIR [options]
#   reliability-gate.sh check-baseline --baseline FILE [--phases LIST]
#
# Options (all sub-commands ignore the ones that do not apply to them):
#   --results DIR       Result directory for this run (holds manifest.tsv,
#                       samples.tsv, summary.tsv). Required by pre/postflight.
#   --baseline FILE     Baseline TSV or baseline JSON to validate.
#   --phases LIST       Comma-separated phase subset (default: every phase in
#                       the baseline / every phase present in samples.tsv).
#   --max-load L        Load-average ceiling (default 2.0). Checked at start
#                       (preflight) and against every recorded per-sample
#                       reading (postflight).
#   --min-samples N     Required sample count per phase (default 7).
#   --max-cv PCT        Coefficient-of-variation ceiling per phase, percent
#                       (default 5).
#   --max-freq-drift PCT  Allowed (max-min)/max spread of the pinned core's
#                       frequency during a run (default 20).
#   --cpu N             CPU the run is/was pinned to (preflight only; taken
#                       from the manifest in postflight).
#   --reps N            Reps the runner is about to perform (preflight only;
#                       refused early when below --min-samples so a bad run is
#                       rejected in one second rather than in 90 minutes).
#   --require-freq-data Treat "frequency data unavailable" as a failure rather
#                       than a warning (off by default: cpufreq sysfs is not
#                       readable in every container).
#   --calibrate         Baseline-threshold checks are skipped (you are
#                       *creating* the baseline, so it cannot yet be valid).
#
# Exit codes — every failure prints `RELIABILITY-FAIL[<CHECK>]: <reason>` on
# stderr first, and every failed check is reported, not just the first:
#    0  every check passed (warnings may still have been printed)
#    2  usage / setup error (bad arguments, unreadable result directory)
#    3  host too loaded or contended to measure  [HOST-LOAD, HOST-CONTENTION]
#   10  checksum drift, checksum != reference, or a run that did not complete
#       [CHECKSUM-DRIFT, CHECKSUM-REFERENCE, SAMPLE-EXIT]
#   11  placeholder / zero / missing baseline used as a threshold
#       [BASELINE-MISSING, BASELINE-PLACEHOLDER, BASELINE-CHECKSUM]
#   12  too few samples  [SAMPLE-COUNT]
#   13  measurement instability  [CPU-PIN, CPU-MIGRATION, CPU-THERMAL,
#       CPU-FREQ, RUN-VARIANCE]
#   14  missing or incomplete environment manifest  [MANIFEST-MISSING,
#       MANIFEST-FIELD, SUMMARY-INTEGRITY]
#
# The exit code is the code of the FIRST failing check in the order above;
# every failure is still printed and written to <results>/reliability.tsv and
# <results>/reliability.json for compare.py to read.
#
# See docs/benchmarking/reliability-gate.md for what each check rejects and
# why, and docs/benchmarking/methodology.md for the protocol it enforces.
set -u

# Every numeric comparison in this script goes through awk. Under a locale
# whose decimal separator is a comma, "9.9" can parse as 9 and a load,
# frequency-drift or CV ceiling silently stops rejecting anything while still
# printing PASS. Pin the numeric locale rather than hope the bench host's is
# the one it was written on. (The PowerShell twin does the same thing with
# InvariantCulture, for the same reason.)
export LC_ALL=C

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCHEMA_VERSION=1

MODE=""
RESULTS=""
BASELINE=""
PHASES=""
MAX_LOAD="2.0"
MIN_SAMPLES=7
MAX_CV="5"
MAX_FREQ_DRIFT="20"
CPU=""
REPS=""
REQUIRE_FREQ=0
CALIBRATE=0

# Manifest keys that must be present and non-empty for a run to be citable.
# "-" counts as absent: the runner writes "-" for anything it could not read,
# and a field it could not read is exactly the field a reader would otherwise
# assume was checked.
REQUIRED_MANIFEST_KEYS="schema_version run_id created_utc host cpu_model cpu_pinned revision binary_path binary_sha256 vm_flags command_line jdk_version baseline_file reps load1_start"

usage() {
    sed -n '2,80p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
    exit 2
}

[ $# -gt 0 ] || usage
MODE="$1"
shift
case "$MODE" in
    preflight|postflight|check-baseline) ;;
    -h|--help|help) usage ;;
    *) echo "FATAL: unknown sub-command: $MODE" >&2; exit 2 ;;
esac

while [ $# -gt 0 ]; do
    case "$1" in
        --results) RESULTS="$2"; shift 2 ;;
        --baseline) BASELINE="$2"; shift 2 ;;
        --phases) PHASES="$2"; shift 2 ;;
        --max-load) MAX_LOAD="$2"; shift 2 ;;
        --min-samples) MIN_SAMPLES="$2"; shift 2 ;;
        --max-cv) MAX_CV="$2"; shift 2 ;;
        --max-freq-drift) MAX_FREQ_DRIFT="$2"; shift 2 ;;
        --cpu) CPU="$2"; shift 2 ;;
        --reps) REPS="$2"; shift 2 ;;
        --require-freq-data) REQUIRE_FREQ=1; shift ;;
        --calibrate) CALIBRATE=1; shift ;;
        *) echo "FATAL: unknown arg: $1" >&2; exit 2 ;;
    esac
done

# ---------------------------------------------------------------------------
# Failure bookkeeping
# ---------------------------------------------------------------------------
# Checks are recorded in the order they run; the process exit code is the
# severity of the first failure, but every failure is printed. A gate that
# stops at the first problem trains people to fix one thing, re-run for 90
# minutes, and find the next.

FAIL_COUNT=0
WARN_COUNT=0
EXIT_CODE=0
CHECK_ROWS=""

record() {  # record <status> <check-id> <detail...>
    local status="$1" id="$2"
    shift 2
    CHECK_ROWS="${CHECK_ROWS}${id}	${status}	$*
"
}

pass_check() { record PASS "$1" "${2:-ok}"; }

warn_check() {  # warn_check <check-id> <detail...>
    local id="$1"; shift
    WARN_COUNT=$((WARN_COUNT + 1))
    echo "RELIABILITY-WARN[$id]: $*" >&2
    record WARN "$id" "$*"
}

fail_check() {  # fail_check <exit-code> <check-id> <detail...>
    local code="$1" id="$2"
    shift 2
    FAIL_COUNT=$((FAIL_COUNT + 1))
    [ "$EXIT_CODE" -eq 0 ] && EXIT_CODE="$code"
    echo "RELIABILITY-FAIL[$id]: $*" >&2
    record FAIL "$id" "$*"
}

json_escape() {
    printf '%s' "$1" | awk '
        { gsub(/\\/, "\\\\"); gsub(/"/, "\\\""); gsub(/\t/, "\\t"); printf "%s", (NR>1 ? "\\n" : "") $0 }
    '
}

# Numeric comparison helpers that do not need bc (not installed on every
# bench host) and do not silently succeed on non-numeric input.
gt() { awk -v a="$1" -v b="$2" 'BEGIN { exit !(a + 0 > b + 0) }'; }
is_num() { printf '%s' "$1" | grep -qE '^-?[0-9]+(\.[0-9]+)?$'; }

# ---------------------------------------------------------------------------
# Manifest access
# ---------------------------------------------------------------------------
MANIFEST=""
mf() {  # mf <key> -> value on stdout, empty when absent
    [ -n "$MANIFEST" ] && [ -f "$MANIFEST" ] || return 1
    awk -F'\t' -v k="$1" '$1 == k { print $2; exit }' "$MANIFEST"
}

check_manifest() {
    MANIFEST="$RESULTS/manifest.tsv"
    if [ ! -f "$MANIFEST" ]; then
        fail_check 14 MANIFEST-MISSING \
            "no environment manifest at $MANIFEST — a run with no recorded revision, binary hash, flags, JDK and CPU model cannot be cited or reproduced"
        return 1
    fi
    local missing="" key value
    for key in $REQUIRED_MANIFEST_KEYS; do
        value="$(mf "$key")"
        case "$value" in
            ""|"-"|"unknown") missing="$missing $key" ;;
        esac
    done
    if [ -n "$missing" ]; then
        fail_check 14 MANIFEST-FIELD \
            "environment manifest is incomplete — missing or unrecorded:$missing (manifest: $MANIFEST)"
        return 1
    fi
    pass_check MANIFEST-FIELD "all $(printf '%s' "$REQUIRED_MANIFEST_KEYS" | wc -w) required manifest fields recorded"
    return 0
}

# ---------------------------------------------------------------------------
# Baseline validation — the placeholder check
# ---------------------------------------------------------------------------
# Two file dialects are accepted:
#
#   TSV  — `phase <TAB> baseline_ms <TAB> checksum <TAB> status <TAB> evidence`,
#          the historical format. A row is a placeholder when baseline_ms is
#          0/empty/non-numeric, when the checksum column is empty, or when the
#          status column is exactly `placeholder`.
#   JSON — `{"phases": {"<phase>": {"baseline_ms": N, "checksum": "...",
#          "status": "...", "placeholder": true}}}`, the machine-readable
#          marker format documented in baselines/README.md. `"placeholder":
#          true` anywhere in a phase object disqualifies that phase.
#
# The JSON reader is deliberately a line-oriented scanner rather than a real
# parser: this must run on a bench host with nothing but coreutils installed,
# and the format is fixed by baselines/TEMPLATE.json.

baseline_phase_rows() {  # -> "phase<TAB>ms<TAB>checksum<TAB>status<TAB>placeholder"
    local file="$1"
    case "$file" in
        *.json)
            awk '
                /"[A-Za-z0-9_]+"[ \t]*:[ \t]*\{/ && !/"phases"/ {
                    if (match($0, /"[A-Za-z0-9_]+"/)) {
                        phase = substr($0, RSTART + 1, RLENGTH - 2)
                        ms[phase] = ""; ck[phase] = ""; st[phase] = ""; ph[phase] = "false"
                        order[++n] = phase
                        cur = phase
                    }
                    next
                }
                cur != "" && /"baseline_ms"/ { if (match($0, /-?[0-9]+(\.[0-9]+)?[ \t]*,?[ \t]*$/)) { v = substr($0, RSTART, RLENGTH); gsub(/[ \t,]/, "", v); ms[cur] = v } }
                cur != "" && /"checksum"/ { if (match($0, /:[ \t]*"[^"]*"/)) { v = substr($0, RSTART, RLENGTH); sub(/^:[ \t]*"/, "", v); sub(/"$/, "", v); ck[cur] = v } }
                cur != "" && /"status"/ { if (match($0, /:[ \t]*"[^"]*"/)) { v = substr($0, RSTART, RLENGTH); sub(/^:[ \t]*"/, "", v); sub(/"$/, "", v); st[cur] = v } }
                cur != "" && /"placeholder"/ { ph[cur] = (/true/ ? "true" : "false") }
                END {
                    for (i = 1; i <= n; i++) {
                        p = order[i]
                        printf "%s\t%s\t%s\t%s\t%s\n", p, ms[p], ck[p], st[p], ph[p]
                    }
                }
            ' "$file"
            ;;
        *)
            awk -F'\t' '
                { sub(/\r$/, "") }   # this repo checks out CRLF on Windows
                /^#/ || NF == 0 { next }
                $1 == "" { next }
                {
                    ph = ($4 == "placeholder" || $4 ~ /placeholder=true/) ? "true" : "false"
                    printf "%s\t%s\t%s\t%s\t%s\n", $1, $2, $3, $4, ph
                }
            ' "$file"
            ;;
    esac
}

wanted_phase() {  # wanted_phase <phase>
    [ -z "$PHASES" ] && return 0
    case ",$PHASES," in *",$1,"*) return 0 ;; *) return 1 ;; esac
}

check_baseline() {
    if [ -z "$BASELINE" ]; then
        fail_check 11 BASELINE-MISSING "no --baseline given; a regression verdict without a baseline is not a verdict"
        return 1
    fi
    if [ ! -f "$BASELINE" ]; then
        fail_check 11 BASELINE-MISSING "baseline file not found: $BASELINE"
        return 1
    fi

    local rows seen=0 bad=0
    rows="$(baseline_phase_rows "$BASELINE")"
    if [ -z "$rows" ]; then
        fail_check 11 BASELINE-MISSING "baseline $BASELINE contains no phase rows"
        return 1
    fi

    local phase ms sum status placeholder
    while IFS=$'\t' read -r phase ms sum status placeholder; do
        [ -n "$phase" ] || continue
        wanted_phase "$phase" || continue
        seen=$((seen + 1))
        if [ "$placeholder" = "true" ]; then
            fail_check 11 BASELINE-PLACEHOLDER \
                "$phase: baseline is marked placeholder (status='$status') in $BASELINE — a placeholder must never act as a regression threshold; record a real one per docs/benchmarking/methodology.md"
            bad=$((bad + 1))
            continue
        fi
        if [ -z "$ms" ] || ! is_num "$ms"; then
            fail_check 11 BASELINE-PLACEHOLDER \
                "$phase: baseline_ms is missing or non-numeric ('$ms') in $BASELINE — this is a placeholder in all but name"
            bad=$((bad + 1))
            continue
        fi
        if ! gt "$ms" 0; then
            fail_check 11 BASELINE-PLACEHOLDER \
                "$phase: baseline_ms is $ms in $BASELINE — a zero/negative baseline makes the budget zero, so the gate stops being able to fail (or fails always) while still reporting a verdict"
            bad=$((bad + 1))
            continue
        fi
        if [ -z "$sum" ] || [ "$sum" = "-" ]; then
            fail_check 11 BASELINE-CHECKSUM \
                "$phase: no reference checksum in $BASELINE — without one, a run can only prove it agreed with itself"
            bad=$((bad + 1))
            continue
        fi
    done <<EOF
$rows
EOF

    if [ "$seen" -eq 0 ]; then
        fail_check 11 BASELINE-MISSING \
            "none of the requested phases (${PHASES:-<all>}) exist in $BASELINE"
        return 1
    fi
    [ "$bad" -eq 0 ] && pass_check BASELINE-PLACEHOLDER "$seen phase baseline(s) in $BASELINE are real, non-zero and carry a reference checksum"
    return 0
}

baseline_checksum_for() {  # baseline_checksum_for <phase>
    [ -f "$BASELINE" ] || return 1
    baseline_phase_rows "$BASELINE" | awk -F'\t' -v p="$1" '$1 == p { print $3; exit }'
}

# ---------------------------------------------------------------------------
# Host state
# ---------------------------------------------------------------------------
read_load1() {
    if [ -r /proc/loadavg ]; then
        cut -d' ' -f1 /proc/loadavg
    else
        echo "-"
    fi
}

check_host_load_now() {
    local load
    load="$(read_load1)"
    if [ "$load" = "-" ] || ! is_num "$load"; then
        warn_check HOST-LOAD "cannot read a 1-minute load average on this host; the load ceiling is unenforced for the start-of-run check"
        return 0
    fi
    if gt "$load" "$MAX_LOAD"; then
        fail_check 3 HOST-LOAD \
            "1-minute load average $load exceeds the ceiling $MAX_LOAD at start of run — a contended host produces garbage passes as well as garbage failures, so this must not measure at all"
        return 1
    fi
    pass_check HOST-LOAD "start-of-run load $load <= $MAX_LOAD"
    return 0
}

check_host_contention() {
    # BENCHMARK.md's own methodology warning: concurrent sessions on the shared
    # bench host run their own CratonBench pinned to the same CPU. Two
    # benchmarks then timeshare one core while every other core is idle, which
    # the load average cannot see. Look for the collision directly.
    command -v pgrep >/dev/null 2>&1 || { warn_check HOST-CONTENTION "pgrep unavailable; cannot check for a competing CratonBench on this CPU"; return 0; }
    local others
    others="$(pgrep -f CratonBench 2>/dev/null | grep -v "^$$\$" | tr '\n' ' ')"
    if [ -n "${others// /}" ]; then
        fail_check 3 HOST-CONTENTION \
            "another CratonBench is already running (pids: ${others% }) — check 'taskset -cp <pid>'; a co-pinned benchmark halves the core without moving the load average"
        return 1
    fi
    pass_check HOST-CONTENTION "no competing CratonBench process"
    return 0
}

check_pin() {
    local cpu="$1"
    if [ -z "$cpu" ] || ! is_num "$cpu"; then
        fail_check 13 CPU-PIN "no single CPU recorded for this run ('$cpu') — an unpinned run cannot be compared with a pinned baseline"
        return 1
    fi
    if command -v taskset >/dev/null 2>&1; then
        pass_check CPU-PIN "pinned to a single logical CPU ($cpu)"
    else
        warn_check CPU-PIN "taskset not available; pin cannot be enforced by this host"
    fi
    return 0
}

# ---------------------------------------------------------------------------
# Sample-level checks (postflight)
# ---------------------------------------------------------------------------
SAMPLES=""
SUMMARY=""

samples_field_index() {  # samples_field_index <column-name>
    awk -F'\t' -v want="$1" '
        /^#/ && NR == 1 {
            for (i = 1; i <= NF; i++) {
                h = $i; sub(/^#/, "", h)
                if (h == want) { print i; exit }
            }
            exit
        }
    ' "$SAMPLES"
}

check_samples_present() {
    SAMPLES="$RESULTS/samples.tsv"
    SUMMARY="$RESULTS/summary.tsv"
    # A UTF-8 BOM (which is what Windows PowerShell's `Set-Content -Encoding
    # utf8` writes) hides the leading '#' of the header row from every
    # `/^#/` test below: the header then reads as a data row and the column
    # names are never found, so checks silently report "no checksum column"
    # instead of checking checksums. Normalise once, loudly.
    if [ -f "$SAMPLES" ] && [ "$(head -c 3 "$SAMPLES" 2>/dev/null)" = "$(printf '\357\273\277')" ]; then
        local stripped="${TMPDIR:-/tmp}/cratonbench-samples-$$.tsv"
        tail -c +4 "$SAMPLES" > "$stripped"
        warn_check SUMMARY-INTEGRITY "samples.tsv starts with a UTF-8 BOM; reading a BOM-stripped copy"
        SAMPLES="$stripped"
    fi
    if [ ! -f "$SAMPLES" ]; then
        fail_check 14 MANIFEST-MISSING \
            "no raw sample file at $SAMPLES — a results directory that kept only the summary cannot be re-analysed, and 'we only kept the median' is how the retracted HashMap number survived"
        return 1
    fi
    return 0
}

phases_in_samples() {
    awk -F'\t' '!/^#/ && NF > 2 { print $1 }' "$SAMPLES" | sort -u
}

check_sample_counts() {
    local phase n bad=0 seen=0
    for phase in $(phases_in_samples); do
        wanted_phase "$phase" || continue
        seen=$((seen + 1))
        n=$(awk -F'\t' -v p="$phase" '!/^#/ && $1 == p { c++ } END { print c + 0 }' "$SAMPLES")
        if [ "$n" -lt "$MIN_SAMPLES" ]; then
            fail_check 12 SAMPLE-COUNT \
                "$phase: $n sample(s) recorded, protocol requires >= $MIN_SAMPLES — a median of fewer runs is not a median, it is a draw"
            bad=$((bad + 1))
        fi
    done
    if [ "$seen" -eq 0 ]; then
        fail_check 12 SAMPLE-COUNT "no samples recorded for the requested phases (${PHASES:-<all>})"
        return 1
    fi
    [ "$bad" -eq 0 ] && pass_check SAMPLE-COUNT "every measured phase has >= $MIN_SAMPLES samples"
    return 0
}

check_exit_codes() {
    local idx bad
    idx=$(samples_field_index exit_code)
    if [ -z "$idx" ]; then
        warn_check SAMPLE-EXIT "samples.tsv has no exit_code column; per-run completion is unverified"
        return 0
    fi
    bad=$(awk -F'\t' -v i="$idx" '!/^#/ && NF > 2 && $i != "0" { printf "%s(rep %s, exit %s) ", $1, $2, $i }' "$SAMPLES")
    if [ -n "$bad" ]; then
        fail_check 10 SAMPLE-EXIT "run(s) did not complete cleanly: ${bad% } — a crashed or timed-out run must never contribute a time"
        return 1
    fi
    pass_check SAMPLE-EXIT "every run exited 0"
    return 0
}

check_checksums() {
    local idx phase distinct ref bad=0
    idx=$(samples_field_index checksum)
    if [ -z "$idx" ]; then
        fail_check 10 CHECKSUM-DRIFT "samples.tsv has no checksum column — an unchecksummed benchmark measures how fast the wrong answer is produced"
        return 1
    fi
    for phase in $(phases_in_samples); do
        wanted_phase "$phase" || continue
        distinct=$(awk -F'\t' -v p="$phase" -v i="$idx" '!/^#/ && $1 == p { print $i }' "$SAMPLES" | sort -u | tr '\n' ' ')
        if [ "$(printf '%s\n' $distinct | wc -l)" -gt 1 ]; then
            fail_check 10 CHECKSUM-DRIFT \
                "$phase: runs disagreed with each other — checksums seen: ${distinct% } — this is a CORRECTNESS regression, not a perf result"
            bad=$((bad + 1))
            continue
        fi
        ref="$(baseline_checksum_for "$phase" 2>/dev/null)"
        if [ -z "$ref" ] || [ "$ref" = "-" ]; then
            fail_check 11 BASELINE-CHECKSUM \
                "$phase: no reference checksum recorded in ${BASELINE:-<no baseline>} to compare ${distinct% } against"
            bad=$((bad + 1))
            continue
        fi
        if [ "${distinct% }" != "$ref" ]; then
            fail_check 10 CHECKSUM-REFERENCE \
                "$phase: checksum ${distinct% } != reference $ref — a faster wrong answer is a bug, not a result"
            bad=$((bad + 1))
        fi
    done
    [ "$bad" -eq 0 ] && pass_check CHECKSUM-REFERENCE "every run of every phase matched its recorded reference checksum"
    return 0
}

check_load_during_run() {
    local idx over
    idx=$(samples_field_index load1)
    if [ -z "$idx" ]; then
        warn_check HOST-LOAD "samples.tsv has no load1 column; load during the run is unverified (only the opening reading was checked)"
        return 0
    fi
    over=$(awk -F'\t' -v i="$idx" -v m="$MAX_LOAD" '
        !/^#/ && NF > 2 && $i != "-" && $i + 0 > m + 0 { printf "%s(rep %s: %s) ", $1, $2, $i }
    ' "$SAMPLES")
    if [ -n "$over" ]; then
        fail_check 3 HOST-LOAD \
            "load rose above $MAX_LOAD during the run: ${over% } — the opening reading passing is not evidence the whole run was quiet"
        return 1
    fi
    local end
    end="$(mf load1_end 2>/dev/null)"
    pass_check HOST-LOAD "every per-sample load reading <= $MAX_LOAD (end of run: ${end:--})"
    return 0
}

check_cpu_stability() {
    local pinned idx_obs idx_thr idx_kmin idx_kmax
    pinned="$(mf cpu_pinned 2>/dev/null)"
    check_pin "$pinned"

    idx_obs=$(samples_field_index cpu_observed)
    if [ -z "$idx_obs" ]; then
        fail_check 13 CPU-MIGRATION "samples.tsv has no cpu_observed column — CPU migration is undetectable, and a migrated run silently measures a different core"
    else
        local moved unobserved mask
        moved=$(awk -F'\t' -v i="$idx_obs" -v p="$pinned" '
            !/^#/ && NF > 2 && $i != "-" && ($i ~ /,/ || $i != p) { printf "%s(rep %s ran on %s) ", $1, $2, $i }
        ' "$SAMPLES")
        unobserved=$(awk -F'\t' -v i="$idx_obs" '!/^#/ && NF > 2 && $i == "-" { c++ } END { print c + 0 }' "$SAMPLES")
        if [ -n "$moved" ]; then
            fail_check 13 CPU-MIGRATION \
                "process did not stay on the pinned CPU $pinned: ${moved% } — cross-core migration changes cache and frequency behaviour mid-measurement"
        elif [ "$unobserved" -gt 0 ]; then
            mask="$(mf cpu_affinity_mask 2>/dev/null)"
            if [ -n "$mask" ] && [ "$mask" = "$pinned" ]; then
                warn_check CPU-MIGRATION \
                    "$unobserved sample(s) had no observed CPU, but the affinity mask was the single CPU $mask, which makes migration impossible"
            else
                fail_check 13 CPU-MIGRATION \
                    "$unobserved sample(s) recorded no observed CPU and the affinity mask ('${mask:--}') is not the single pinned CPU $pinned — migration cannot be ruled out"
            fi
        else
            pass_check CPU-MIGRATION "every sample ran on the pinned CPU $pinned"
        fi
    fi

    idx_thr=$(samples_field_index throttle_delta)
    if [ -z "$idx_thr" ]; then
        warn_check CPU-THERMAL "samples.tsv has no throttle_delta column; thermal throttling is unverified"
    else
        local thr
        thr=$(awk -F'\t' -v i="$idx_thr" '!/^#/ && NF > 2 && $i != "-" && $i + 0 > 0 { printf "%s(rep %s: +%s) ", $1, $2, $i }' "$SAMPLES")
        if [ -n "$thr" ]; then
            fail_check 13 CPU-THERMAL \
                "the pinned core throttled during the run: ${thr% } — a thermally-limited core is not the core the baseline was measured on"
        else
            pass_check CPU-THERMAL "no thermal-throttle events on the pinned core"
        fi
    fi

    idx_kmin=$(samples_field_index khz_min)
    idx_kmax=$(samples_field_index khz_max)
    if [ -z "$idx_kmin" ] || [ -z "$idx_kmax" ]; then
        if [ "$REQUIRE_FREQ" -eq 1 ]; then
            fail_check 13 CPU-FREQ "no frequency columns in samples.tsv and --require-freq-data was given"
        else
            warn_check CPU-FREQ "samples.tsv has no frequency columns; core-frequency stability is unverified (pass --require-freq-data to make this fatal)"
        fi
    else
        local unavailable drift
        unavailable=$(awk -F'\t' -v a="$idx_kmin" -v b="$idx_kmax" '!/^#/ && NF > 2 && ($a == "-" || $b == "-") { c++ } END { print c + 0 }' "$SAMPLES")
        drift=$(awk -F'\t' -v a="$idx_kmin" -v b="$idx_kmax" -v m="$MAX_FREQ_DRIFT" '
            !/^#/ && NF > 2 && $a != "-" && $b != "-" && $b + 0 > 0 {
                d = ($b - $a) * 100.0 / $b
                if (d > m + 0) printf "%s(rep %s: %.1f%%) ", $1, $2, d
            }
        ' "$SAMPLES")
        if [ -n "$drift" ]; then
            fail_check 13 CPU-FREQ \
                "pinned-core frequency moved more than ${MAX_FREQ_DRIFT}% within a run: ${drift% } — boost/thermal drift of that size is larger than most regressions this gate is asked to detect"
        elif [ "$unavailable" -gt 0 ] && [ "$REQUIRE_FREQ" -eq 1 ]; then
            fail_check 13 CPU-FREQ "$unavailable sample(s) had no readable cpufreq data and --require-freq-data was given"
        elif [ "$unavailable" -gt 0 ]; then
            warn_check CPU-FREQ "$unavailable sample(s) had no readable cpufreq data (scaling_cur_freq not exported); frequency stability is unverified for those"
        else
            pass_check CPU-FREQ "pinned-core frequency spread within ${MAX_FREQ_DRIFT}% on every sample"
        fi
    fi
}

check_variance() {
    local idx bad
    idx=$(samples_field_index ms)
    if [ -z "$idx" ]; then
        fail_check 14 SUMMARY-INTEGRITY "samples.tsv has no ms column"
        return 1
    fi
    bad=$(awk -F'\t' -v i="$idx" -v m="$MAX_CV" -v want="$PHASES" '
        function wanted(p) { return (want == "" || index("," want ",", "," p ",") > 0) }
        !/^#/ && NF > 2 && wanted($1) { n[$1]++; s[$1] += $i; q[$1] += $i * $i }
        END {
            for (p in n) {
                if (n[p] < 2) continue
                mean = s[p] / n[p]
                var = (q[p] - n[p] * mean * mean) / (n[p] - 1)
                if (var < 0) var = 0
                cv = (mean > 0) ? 100.0 * sqrt(var) / mean : 0
                if (cv > m + 0) printf "%s(CV %.2f%%, mean %.1f ms, n=%d) ", p, cv, mean, n[p]
            }
        }
    ' "$SAMPLES")
    if [ -n "$bad" ]; then
        fail_check 13 RUN-VARIANCE \
            "run-to-run variance exceeds ${MAX_CV}%: ${bad% } — that spread is the signature of an unstable host (frequency, co-tenancy, thermal), and a median drawn from it cannot resolve the regression sizes this gate is for"
        return 1
    fi
    pass_check RUN-VARIANCE "every phase's coefficient of variation <= ${MAX_CV}%"
    return 0
}

check_summary_integrity() {
    # The summary must be derivable from the raw samples. A summary that
    # disagrees with its own samples means either a recording bug or a
    # hand-edited result, and either way the distribution is fiction.
    [ -f "$SUMMARY" ] || { fail_check 14 SUMMARY-INTEGRITY "no summary.tsv in $RESULTS"; return 1; }
    local idx_ms bad
    idx_ms=$(samples_field_index ms)
    bad=$(awk -F'\t' -v ims="$idx_ms" -v samples="$SAMPLES" -v want="$PHASES" '
        function wanted(p) { return (want == "" || index("," want ",", "," p ",") > 0) }
        BEGIN {
            while ((getline line < samples) > 0) {
                if (line ~ /^#/) continue
                nf = split(line, f, "\t")
                if (nf < 3) continue
                p = f[1]
                cnt[p]++
                vals[p, cnt[p]] = f[ims] + 0
            }
            close(samples)
            for (p in cnt) {
                n = cnt[p]
                for (i = 2; i <= n; i++) {
                    v = vals[p, i]; j = i - 1
                    while (j > 0 && vals[p, j] > v) { vals[p, j + 1] = vals[p, j]; j-- }
                    vals[p, j + 1] = v
                }
                # Nearest-rank p50, the same definition the runner and the G1
                # pause summary use.
                r = int((50 * n + 99) / 100); if (r < 1) r = 1
                p50[p] = vals[p, r]
                nn[p] = n
            }
        }
        /^#/ { next }
        NF > 2 && wanted($1) {
            if (!($1 in nn)) { printf "%s(in summary, absent from samples) ", $1; next }
            if ($2 + 0 != nn[$1]) { printf "%s(summary n=%s, samples n=%d) ", $1, $2, nn[$1]; next }
            if ($4 + 0 != p50[$1]) { printf "%s(summary p50=%s, samples p50=%d) ", $1, $4, p50[$1] }
        }
    ' "$SUMMARY")
    if [ -n "$bad" ]; then
        fail_check 14 SUMMARY-INTEGRITY \
            "summary.tsv does not match the raw samples: ${bad% } — the raw samples are the record; a summary that cannot be rederived from them is not evidence"
        return 1
    fi
    pass_check SUMMARY-INTEGRITY "summary.tsv rederives exactly from samples.tsv (n and p50)"
    return 0
}

# ---------------------------------------------------------------------------
# Report writing
# ---------------------------------------------------------------------------
write_report() {
    [ -n "$RESULTS" ] && [ -d "$RESULTS" ] || return 0
    local status="pass"
    [ "$FAIL_COUNT" -gt 0 ] && status="fail"

    {
        printf '# CratonBench reliability gate report — schema %s\n' "$SCHEMA_VERSION"
        printf '# check\tstatus\tdetail\n'
        printf 'gate.mode\t%s\t%s\n' "$MODE" "-"
        printf 'gate.status\t%s\texit=%s fail=%s warn=%s\n' "$status" "$EXIT_CODE" "$FAIL_COUNT" "$WARN_COUNT"
        printf '%s' "$CHECK_ROWS"
    } > "$RESULTS/reliability-$MODE.tsv"

    {
        printf '{\n'
        printf '  "schema_version": %s,\n' "$SCHEMA_VERSION"
        printf '  "mode": "%s",\n' "$MODE"
        printf '  "status": "%s",\n' "$status"
        printf '  "exit_code": %s,\n' "$EXIT_CODE"
        printf '  "failures": %s,\n' "$FAIL_COUNT"
        printf '  "warnings": %s,\n' "$WARN_COUNT"
        printf '  "min_samples": %s,\n' "$MIN_SAMPLES"
        printf '  "max_load": "%s",\n' "$MAX_LOAD"
        printf '  "max_cv_pct": "%s",\n' "$MAX_CV"
        printf '  "checks": [\n'
        printf '%s' "$CHECK_ROWS" | awk -F'\t' '
            NF >= 2 {
                d = $3
                gsub(/\\/, "\\\\", d); gsub(/"/, "\\\"", d)
                rows[++n] = sprintf("    {\"id\": \"%s\", \"status\": \"%s\", \"detail\": \"%s\"}", $1, $2, d)
            }
            END { for (i = 1; i <= n; i++) printf "%s%s\n", rows[i], (i < n ? "," : "") }
        '
        printf '  ]\n'
        printf '}\n'
    } > "$RESULTS/reliability-$MODE.json"

    # `reliability.json` always names the LATEST decision, which is what
    # compare.py reads. Postflight overwrites preflight deliberately: a run
    # whose preflight passed and whose postflight failed is a failed run.
    cp "$RESULTS/reliability-$MODE.json" "$RESULTS/reliability.json" 2>/dev/null
    cp "$RESULTS/reliability-$MODE.tsv" "$RESULTS/reliability.tsv" 2>/dev/null
}

# ---------------------------------------------------------------------------
# Sub-commands
# ---------------------------------------------------------------------------
case "$MODE" in
    check-baseline)
        check_baseline
        ;;

    preflight)
        [ -n "$RESULTS" ] && [ -d "$RESULTS" ] || { echo "FATAL: --results DIR required and must exist" >&2; exit 2; }
        echo "reliability-gate preflight: results=$RESULTS min-samples=$MIN_SAMPLES max-load=$MAX_LOAD"
        check_manifest
        # The reps check is here and not in postflight so that a run that
        # cannot possibly satisfy the protocol is rejected before it burns an
        # hour of bench-host time.
        if [ -n "$REPS" ] && is_num "$REPS" && [ "$REPS" -lt "$MIN_SAMPLES" ]; then
            fail_check 12 SAMPLE-COUNT \
                "requested --reps $REPS is below the required $MIN_SAMPLES samples per phase; raise --reps, or lower --min-samples explicitly and say so in the write-up"
        else
            pass_check SAMPLE-COUNT "planned reps ${REPS:-<unset>} >= required $MIN_SAMPLES"
        fi
        check_host_load_now
        check_host_contention
        check_pin "${CPU:-$(mf cpu_pinned 2>/dev/null)}"
        if [ "$CALIBRATE" -eq 1 ]; then
            warn_check BASELINE-PLACEHOLDER "--calibrate: baseline threshold checks skipped because this run is recording a baseline rather than being gated by one"
        else
            check_baseline
        fi
        ;;

    postflight)
        [ -n "$RESULTS" ] && [ -d "$RESULTS" ] || { echo "FATAL: --results DIR required and must exist" >&2; exit 2; }
        echo "reliability-gate postflight: results=$RESULTS min-samples=$MIN_SAMPLES max-load=$MAX_LOAD max-cv=${MAX_CV}%"
        check_manifest
        if check_samples_present; then
            check_sample_counts
            check_exit_codes
            if [ "$CALIBRATE" -eq 1 ]; then
                warn_check BASELINE-PLACEHOLDER "--calibrate: baseline threshold checks skipped; checksums are still compared between runs below"
                # Even while calibrating, runs must agree with each other.
                local_idx=$(samples_field_index checksum)
                if [ -n "$local_idx" ]; then
                    for p in $(phases_in_samples); do
                        wanted_phase "$p" || continue
                        d=$(awk -F'\t' -v p="$p" -v i="$local_idx" '!/^#/ && $1 == p { print $i }' "$SAMPLES" | sort -u | tr '\n' ' ')
                        if [ "$(printf '%s\n' $d | wc -l)" -gt 1 ]; then
                            fail_check 10 CHECKSUM-DRIFT "$p: runs disagreed with each other — checksums seen: ${d% }"
                        fi
                    done
                fi
            else
                check_baseline
                check_checksums
            fi
            check_load_during_run
            check_cpu_stability
            check_variance
            check_summary_integrity
        fi
        ;;
esac

write_report

echo "---------------------------------------------"
if [ "$FAIL_COUNT" -gt 0 ]; then
    echo "RELIABILITY GATE ($MODE): FAILED — $FAIL_COUNT check(s), $WARN_COUNT warning(s); exit $EXIT_CODE"
    echo "This run is NOT usable as evidence. See docs/benchmarking/reliability-gate.md."
    exit "$EXIT_CODE"
fi
echo "RELIABILITY GATE ($MODE): PASS ($WARN_COUNT warning(s))"
exit 0
