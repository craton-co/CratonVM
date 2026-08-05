#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
#
# bridge-ratchet.sh — THE L6 GATE, run where a JDK exists.
#
# See docs/internal/L6-unadjudicated-bridge-ratchet-DONE-20260805.md (the lane
# brief, retired 2026-08-05 when this landed) and
# docs/known-issues/jdk-only/native-kind-is-ambient-and-defaults-to-syntheticstub.md.
#
# Contract §1.5 defines a `Bridge` as what an `ACC_NATIVE` method binds to.
# 10,084 of the 10,844 `Bridge` registrations have no such target, every one of
# them inherited its kind from an ambient `set_category`, and nothing stopped
# that number rising. This boots the VM against a real JDK image, takes the
# schema-3 census, and scores it against the baseline committed in
# `scripts/baselines/jdk-only-bridge-ratchet.json`.
#
# It lives here, and not in `native-builtins/tests/`, for one reason: the
# question needs a real JDK image at measurement time and `cargo test` has none.
# `scripts/jdk-only-bridge-ratchet.py`'s docstring records that choice and why
# the committed-artefact alternative was rejected.
#
# The gate's own logic is exercised hermetically on every run first
# (`--selftest`), including the injection the lane doc asks for: a `Bridge`
# registration onto a method with concrete bytecode must make it fail. A guard
# never shown to fail is decoration — three shipped inert in this feature.
#
# Usage:
#   sh regression-suite/bridge-ratchet.sh
#   sh regression-suite/bridge-ratchet.sh --selftest      # hermetic; no VM, no JDK
#   sh regression-suite/bridge-ratchet.sh --update-baseline --note "why"
#   CV=<cratonvm> JAVA_HOME=<jdk25> sh regression-suite/bridge-ratchet.sh
#
# Environment:
#   CV          cratonvm launcher (default: target/{release,debug}/cratonvm[.exe])
#   JAVA_HOME   real JDK runtime image (or JDK=, which run.sh also honours)
#   OUT         scratch directory (default: target/bridge-ratchet)
#   PYTHON      python interpreter (default: python3, then python)
#   JDK_FEATURE override the detected JDK feature version
#   TIMEOUT     seconds before the census run is killed (default 300)
#
# $OUT is never cleaned up on the way out: the census and the adjudication block
# are the evidence for whatever the gate just said.
#
# Exit codes — deliberately the gate script's own, passed through unchanged:
#   0  the ratchet passed
#   1  THE GATE FIRED — an unadjudicated Bridge registration was added
#   2  refused to adjudicate (no baseline for this JDK, census not adjudicated,
#      wrong policy) — never a silent pass
#   3  a prerequisite is missing (no cratonvm binary, no JDK, no python)
set -eu

ROOT="$(git -C "$(dirname "$0")" rev-parse --show-toplevel 2>/dev/null || true)"
[ -n "$ROOT" ] || ROOT="$(cd "$(dirname "$0")/.." && pwd)"
GATE="$ROOT/scripts/jdk-only-bridge-ratchet.py"

# MSYS/Git Bash rewrites arguments that look like POSIX paths; cratonvm needs
# them verbatim. Same reason as scripts/jdk-only-census.sh and run.sh.
MSYS2_ARG_CONV_EXCL='*'; MSYS_NO_PATHCONV=1
export MSYS2_ARG_CONV_EXCL MSYS_NO_PATHCONV

find_python() {
    if [ -n "${PYTHON:-}" ]; then printf '%s\n' "$PYTHON"; return 0; fi
    for c in python3 python; do
        # `command -v` is not enough on Windows: the Store's `python3` alias is
        # on PATH, prints an advertisement and exits non-zero.
        if command -v "$c" >/dev/null 2>&1 && "$c" -c 'import sys' >/dev/null 2>&1; then
            printf '%s\n' "$c"; return 0
        fi
    done
    return 1
}

winpath() { if command -v cygpath >/dev/null 2>&1; then cygpath -m "$1"; else printf '%s\n' "$1"; fi; }

if PY="$(find_python)"; then :; else
    echo "ERROR: no python interpreter found (tried \$PYTHON, python3, python)." >&2
    exit 3
fi

# --- arguments -------------------------------------------------------------
SELFTEST_ONLY=0
UPDATE=0
NOTE=""
while [ "$#" -gt 0 ]; do
    case "$1" in
        --selftest) SELFTEST_ONLY=1; shift ;;
        --update-baseline) UPDATE=1; shift ;;
        --note) NOTE="${2:-}"; shift 2 ;;
        -h|--help) sed -n '5,/^set -eu/p' "$0" | sed -e '/^set -eu$/d' -e 's/^#$//' -e 's/^# //'; exit 0 ;;
        *) echo "ERROR: unknown argument: $1" >&2; exit 3 ;;
    esac
done

# --- the gate's own self-test, always, before anything else ----------------
# Hermetic: no JDK, no VM, no baseline file. It injects an unadjudicated
# `Bridge` into a synthetic census and requires the gate to fail on it. If this
# does not pass, nothing below is evidence of anything.
echo "== bridge-ratchet self-test (hermetic) =="
"$PY" "$(winpath "$GATE")" --selftest || {
    echo "ERROR: the gate's own self-test failed — the census result below would" >&2
    echo "       be meaningless, so it was not taken." >&2
    exit 3
}
[ "$SELFTEST_ONLY" -eq 0 ] || exit 0

# --- locate the launcher ---------------------------------------------------
find_cv() {
    if [ -n "${CV:-}" ]; then printf '%s\n' "$CV"; return; fi
    for c in "$ROOT/target/release/cratonvm" "$ROOT/target/release/cratonvm.exe" \
             "$ROOT/target/debug/cratonvm"   "$ROOT/target/debug/cratonvm.exe"; do
        if [ -x "$c" ]; then printf '%s\n' "$c"; return; fi
    done
}
CV="$(find_cv)"
# An explicit CV= naming the .exe form on a Linux host: run.sh does the same
# fallback, and the build host is where these gates are usually first run.
if [ -n "$CV" ] && [ ! -x "$CV" ]; then
    case "$CV" in
        *.exe) if [ -x "${CV%.exe}" ]; then CV="${CV%.exe}"; fi ;;
    esac
fi
if [ -z "$CV" ] || [ ! -x "$CV" ]; then
    echo "ERROR: cratonvm binary not found (looked under $ROOT/target/{release,debug})." >&2
    echo "       Build it with 'cargo build --release -p cratonvm-cli', or set CV=<path>." >&2
    exit 3
fi

# --- locate a JDK ----------------------------------------------------------
# No degraded mode. Without a real image every `image_declaring_method` is
# meaningless and the gate would score a table of zeroes — which is precisely
# the shape of a clean result.
JDK="${JAVA_HOME:-${JDK:-}}"
if [ -z "$JDK" ]; then
    echo "ERROR: JAVA_HOME is not set. This gate adjudicates registrations against" >&2
    echo "       a real JDK runtime image; there is no useful answer without one." >&2
    exit 3
fi
if [ ! -d "$JDK/jmods" ] && [ ! -f "$JDK/lib/modules" ]; then
    echo "ERROR: $JDK does not look like a JDK runtime image (no jmods/, no lib/modules)." >&2
    exit 3
fi
JAVAC="$JDK/bin/javac"; [ -x "$JAVAC" ] || JAVAC="$JDK/bin/javac.exe"
[ -x "$JAVAC" ] || { echo "ERROR: javac not found under $JDK/bin." >&2; exit 3; }

# --- JDK feature version ---------------------------------------------------
# Mirrors `detect_jdk_feature` in vm-cli/src/main.rs and scripts/jdk-only-census.sh.
read_jdk_version() {
    if [ -f "$1/release" ]; then
        sed -n 's/^JAVA_VERSION=//p' "$1/release" 2>/dev/null | head -n 1 | tr -d '"' | tr -d '\r'
    fi
}
detect_jdk_feature() {
    raw="$(read_jdk_version "$1")"
    [ -n "$raw" ] || return 1
    case "$raw" in 1.*) raw="${raw#1.}" ;; esac
    feature="${raw%%[!0-9]*}"
    [ -n "$feature" ] || return 1
    printf '%s\n' "$feature"
}
JDK_FULL="$(read_jdk_version "$JDK")"
if [ -n "${JDK_FEATURE:-}" ]; then FEATURE="$JDK_FEATURE"
elif FEATURE="$(detect_jdk_feature "$JDK")"; then :
else
    echo "ERROR: could not read JAVA_VERSION from $JDK/release. The baseline is keyed" >&2
    echo "       by feature version; guessing it would score against the wrong image." >&2
    exit 3
fi

# --- the workload ----------------------------------------------------------
# DELIBERATELY TRIVIAL, and the reason is a measurement, not taste.
#
# Neither number this gate freezes depends on the workload. Every registration
# is made inside `SharedVm::new` (`vm/src/vm/vm_init.rs`) before `main` runs, so
# `counts` is a property of the boot, not of the program; and
# `image_declaring_method` is answered by parsing class-path bytes
# (`ClassManager::adjudicate_natives_against_image`), which never loads
# anything and so cannot depend on what the program touched. The census's one
# workload-dependent column, `invocations`, is not read here.
#
# Verified rather than assumed, 2026-08-05 on JDK 25.0.3 / linux: the
# adjudication block from `probes/JdkOnlyCensusLoadProbe` — the breadth-first
# workload the schema-3 census was designed around, 401 slots dispatched — is
# byte-identical to this one-line probe's.
#
# What that buys is not speed. `JdkOnlyCensusLoadProbe` opens sockets, resolves
# DNS and runs executors; it hung in its `net` section on 1 of 3 runs on a
# loaded build host, and a hung probe writes no census, which this script then
# has to report as a missing prerequisite. A gate that flakes on a workload
# whose output it does not read is a gate people learn to ignore.
#
# The probe is generated into a scratch directory of its OWN, and that
# directory alone is the class path. That is load-bearing too:
# `find_class_bytes_delegated` searches the application class path as well as
# the image, so pointing `-cp` at a shared directory would make
# `image_has_class` depend on whatever was lying there and drift per machine.
OUT="${OUT:-$ROOT/target/bridge-ratchet}"
PROBE_CLASS="BridgeRatchetCensusProbe"
WORKLOAD="generated $PROBE_CLASS (boot + one println); -cp is its own directory only"
rm -rf "$OUT"; mkdir -p "$OUT/classes"
cat > "$OUT/$PROBE_CLASS.java" <<EOF
public final class $PROBE_CLASS {
    public static void main(String[] args) {
        System.out.println("bridge-ratchet-census-probe");
    }
}
EOF
"$JAVAC" -d "$OUT/classes" "$OUT/$PROBE_CLASS.java" \
    || { echo "ERROR: javac failed on the generated census probe." >&2; exit 3; }

# --- take the census -------------------------------------------------------
# `--explain-jdk-only` is not optional: without it `image_adjudication` is
# false, every `image_declaring_method` is null, and the gate refuses (exit 2)
# rather than scoring zeroes.
#
# A non-zero VM exit is NOT by itself a failure here — the census is written on
# every exit path — but it is reported, and a census that never appeared is.
#
# Wrapped in `timeout` where one exists: a wedged VM must fail the gate, not
# wedge the job. run.sh does the same for the same reason.
echo ""
echo "== census: --real-jdk against JDK ${JDK_FULL:-$FEATURE} =="
TIMEOUT="${TIMEOUT:-300}"
if command -v timeout >/dev/null 2>&1; then
    set -- timeout "$TIMEOUT" "$CV"
else
    echo "   NOTE: no 'timeout' on PATH — a wedged VM will not be cut off."
    set -- "$CV"
fi
rc=0
"$@" --real-jdk --java-home "$JDK" --explain-jdk-only \
     --dump-native-registry "$OUT/census.json" \
     -cp "$OUT/classes" "$PROBE_CLASS" \
     > "$OUT/probe.stdout.txt" 2> "$OUT/probe.stderr.txt" || rc=$?
echo "   vm exit=$rc  (stdout/stderr in $OUT/probe.*.txt)"
if [ "$rc" -eq 124 ]; then
    echo "ERROR: the census run exceeded TIMEOUT=${TIMEOUT}s and was killed." >&2
    echo "       See $OUT/probe.stderr.txt. This is a VM hang, not a ratchet result." >&2
    exit 3
fi
if [ ! -s "$OUT/census.json" ]; then
    echo "ERROR: no census was written to $OUT/census.json (vm exit $rc)." >&2
    echo "       See $OUT/probe.stderr.txt." >&2
    exit 3
fi
if [ "$rc" -ne 0 ]; then
    # The census is written on every exit path, so a census exists and the
    # numbers below are real. Say so anyway: this probe boots the VM and prints
    # one line, so a non-zero exit is a bootstrap defect worth someone's time
    # even though it does not change the ratchet's answer.
    echo "   NOTE: the census probe exited $rc. It only boots the VM and prints one"
    echo "         line, so that is a real bootstrap failure — see $OUT/probe.stderr.txt."
fi

# --- gate ------------------------------------------------------------------
# `--emit-json` first, so the machine-readable block is on disk as an artefact
# even when the ratchet then fires.
"$PY" "$(winpath "$GATE")" --census "$(winpath "$OUT/census.json")" \
      --emit-json "$(winpath "$OUT/adjudication.json")"
echo ""

set -- \
    --census      "$(winpath "$OUT/census.json")" \
    --baseline    "$(winpath "$ROOT/scripts/baselines/jdk-only-bridge-ratchet.json")" \
    --jdk-feature "$FEATURE" \
    --jdk-version "${JDK_FULL:-}" \
    --workload    "$WORKLOAD"
if [ "$UPDATE" -eq 1 ]; then set -- "$@" --update-baseline; fi
if [ -n "$NOTE" ]; then set -- "$@" --note "$NOTE"; fi

set +e
"$PY" "$(winpath "$GATE")" "$@"
gate_rc=$?
set -e

echo ""
echo "   census        : $OUT/census.json"
echo "   adjudication  : $OUT/adjudication.json"
exit "$gate_rc"
