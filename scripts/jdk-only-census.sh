#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
#
# jdk-only-census.sh — produce the JDK-only mode audit artifacts.
#
# See docs/feature-designs/jdk-only-mode.md. Four files land in
# target/jdk-only-audit/ (override with OUT=<dir>):
#
#   registry-real.json           native registry census, --real-jdk (permissive)
#   registry-no-stubs.json       native registry census, --jdk-only (strict)
#   missing-real-by-module.json  ACC_NATIVE methods with no Rust implementation,
#                                grouped by module — the strict-mode work list
#   class-origins.json           per-class provenance census (--dump-class-origins)
#
# Plus, when the launcher supports it, jdk-only-report.json: the structured
# violation/counter report from the strict run.
#
# The two registry censuses are the point of the whole script. Diffing them
# answers the only question that matters for the 157-stub backlog: which
# registrations does strict mode have to throw away, and does anything the VM
# actually needs disappear with them. Neither file is meaningful alone.
#
# POSIX sh on purpose (no bashisms): this runs from CI on ubuntu-latest and
# windows-latest, and from Git Bash / MSYS on a developer machine.
#
# Usage:
#   sh scripts/jdk-only-census.sh
#   CV=/path/to/cratonvm JAVA_HOME=/path/to/jdk sh scripts/jdk-only-census.sh
#   OUT=/tmp/audit sh scripts/jdk-only-census.sh
#
# Exit codes:
#   0  every artifact was produced
#   3  a prerequisite is missing (no cratonvm binary, no JDK) — nothing ran
#   4  the VM ran but at least one artifact was not written
#
# A non-zero *VM* exit is NOT an error here. Wave 1 expects --jdk-only runs to
# fail on real workloads; the census is exactly how that failure is measured.
# Only a missing artifact is a script failure.

set -eu

# --- locate the repository -------------------------------------------------
ROOT="$(git -C "$(dirname "$0")" rev-parse --show-toplevel 2>/dev/null || true)"
if [ -z "$ROOT" ]; then
    ROOT="$(cd "$(dirname "$0")/.." && pwd)"
fi

OUT="${OUT:-$ROOT/target/jdk-only-audit}"
mkdir -p "$OUT"

# --- locate the cratonvm launcher ------------------------------------------
# Release first: the in-tree test helpers prefer release over debug, and a
# debug binary is slow enough that a full boot census is unpleasant. Both are
# accepted so a developer who only ran `cargo build` still gets artifacts.
find_cv() {
    if [ -n "${CV:-}" ]; then
        printf '%s\n' "$CV"
        return
    fi
    for candidate in \
        "$ROOT/target/release/cratonvm" \
        "$ROOT/target/release/cratonvm.exe" \
        "$ROOT/target/debug/cratonvm" \
        "$ROOT/target/debug/cratonvm.exe"
    do
        if [ -x "$candidate" ]; then
            printf '%s\n' "$candidate"
            return
        fi
    done
}

CV="$(find_cv)"
if [ -z "$CV" ] || [ ! -x "$CV" ]; then
    echo "ERROR: cratonvm binary not found (looked under $ROOT/target/{release,debug})." >&2
    echo "       Build it with 'cargo build --release -p cratonvm-cli', or set CV=<path>." >&2
    exit 3
fi

# --- locate a JDK ----------------------------------------------------------
# --jdk-only *requires* a real runtime image and refuses to start without one,
# so there is no useful degraded mode here: without a JDK this script would
# produce two empty censuses and a false all-clear. That is precisely the hole
# the advisory native-stub census job has today, so fail loudly instead.
JDK="${JAVA_HOME:-}"
if [ -z "$JDK" ]; then
    echo "ERROR: JAVA_HOME is not set." >&2
    echo "       --jdk-only requires a real JDK runtime image (a directory" >&2
    echo "       containing lib/modules or jmods/). Set JAVA_HOME and re-run." >&2
    exit 3
fi
if [ ! -d "$JDK/jmods" ] && [ ! -f "$JDK/lib/modules" ]; then
    echo "ERROR: $JDK does not look like a JDK runtime image" >&2
    echo "       (no jmods/ directory and no lib/modules jimage)." >&2
    exit 3
fi

JAVAC="$JDK/bin/javac"
[ -x "$JAVAC" ] || JAVAC="$JDK/bin/javac.exe"
if [ ! -x "$JAVAC" ]; then
    echo "ERROR: javac not found under $JDK/bin." >&2
    exit 3
fi

# --- the workload ----------------------------------------------------------
# Deliberately trivial and deliberately *not* empty. The censuses describe the
# state of a booted VM, so the program only has to get the VM through boot and
# one ordinary allocation + string + collection path; anything larger makes the
# artifacts workload-specific and the run-to-run diff noisy.
#
# Override with PROBE_CP / PROBE_CLASS to census a real application instead.
PROBE_CP="${PROBE_CP:-}"
PROBE_CLASS="${PROBE_CLASS:-}"

if [ -z "$PROBE_CP" ]; then
    PROBE_SRC="$OUT/probe"
    mkdir -p "$PROBE_SRC"
    cat > "$PROBE_SRC/JdkOnlyCensusProbe.java" <<'EOF'
public final class JdkOnlyCensusProbe {
    public static void main(String[] args) {
        java.util.List<String> parts = new java.util.ArrayList<>();
        for (int i = 0; i < 4; i++) {
            parts.add("p" + i);
        }
        java.util.Map<String, Integer> lengths = new java.util.HashMap<>();
        for (String p : parts) {
            lengths.put(p, p.length());
        }
        System.out.println("census-probe " + lengths.size());
    }
}
EOF
    "$JAVAC" -d "$PROBE_SRC" "$PROBE_SRC/JdkOnlyCensusProbe.java"
    PROBE_CP="$PROBE_SRC"
    PROBE_CLASS="JdkOnlyCensusProbe"
fi
: "${PROBE_CLASS:?PROBE_CLASS must be set when PROBE_CP is}"

# --- run -------------------------------------------------------------------
# MSYS/Git Bash rewrites arguments that look like POSIX paths; the regression
# suite hits the same thing.
MSYS2_ARG_CONV_EXCL='*'
MSYS_NO_PATHCONV=1
export MSYS2_ARG_CONV_EXCL MSYS_NO_PATHCONV

run_vm() {
    label="$1"
    shift
    echo "== $label =="
    rc=0
    # shellcheck disable=SC2086
    "$CV" --java-home "$JDK" "$@" -cp "$PROBE_CP" "$PROBE_CLASS" \
        > "$OUT/$label.stdout.txt" 2> "$OUT/$label.stderr.txt" || rc=$?
    echo "   exit=$rc  (stdout/stderr captured in $OUT/$label.*.txt)"
    return 0
}

# 1. Permissive real-JDK run: the registry as the VM ships it today, plus the
#    missing-native work list. This is the "before" half of the diff.
run_vm real-jdk \
    --real-jdk \
    --XX:AuditMissingNatives \
    --dump-native-registry "$OUT/registry-real.json" \
    --dump-missing-natives-grouped "$OUT/missing-real-by-module.json"

# 2. Strict run: the registry with every SyntheticStub refused, the class-origin
#    census, and the structured violation report.
run_vm jdk-only \
    --jdk-only \
    --dump-native-registry "$OUT/registry-no-stubs.json" \
    --dump-class-origins "$OUT/class-origins.json" \
    --jdk-only-report "$OUT/jdk-only-report.json"

# --- verify ----------------------------------------------------------------
missing=""
for artifact in \
    registry-real.json \
    registry-no-stubs.json \
    missing-real-by-module.json \
    class-origins.json
do
    if [ -s "$OUT/$artifact" ]; then
        echo "ok   $artifact"
    else
        echo "MISS $artifact" >&2
        missing="$missing $artifact"
    fi
done

# jdk-only-report.json is reported but not required: it is only written when a
# strict run gets far enough to have something to report.
if [ -s "$OUT/jdk-only-report.json" ]; then
    echo "ok   jdk-only-report.json"
else
    echo "note jdk-only-report.json not written (optional)"
fi

if [ -n "$missing" ]; then
    echo "" >&2
    echo "ERROR: missing audit artifact(s):$missing" >&2
    echo "       See $OUT/*.stderr.txt for what the VM said." >&2
    exit 4
fi

# --- summarise -------------------------------------------------------------
# grep, not a JSON parser: the dumps are hand-rolled with one key per line, no
# dependency is worth adding for two integers, and CI re-derives these numbers
# itself rather than trusting this summary.
stub_count() {
    grep -o '"synthetic-stub": *[0-9]*' "$1" 2>/dev/null \
        | head -n 1 | tr -dc '0-9'
}

real_stubs="$(stub_count "$OUT/registry-real.json")"
strict_stubs="$(stub_count "$OUT/registry-no-stubs.json")"
compat_classes="$(grep -c '"compatibility-stub"' "$OUT/class-origins.json" 2>/dev/null || true)"

echo ""
echo "== jdk-only census =="
echo "   synthetic-stub natives, --real-jdk : ${real_stubs:-?}"
echo "   synthetic-stub natives, --jdk-only : ${strict_stubs:-?}   (target: 0)"
echo "   compatibility-stub classes         : ${compat_classes:-?}   (target: 0)"
echo "   artifacts in                       : $OUT"
