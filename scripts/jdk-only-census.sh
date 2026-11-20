#!/bin/sh
# SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
# Copyright Craton Software Company
#
# Capture native registries, class origins, missing natives, and policy reports
# for foundational execution and the compatible optimization layer.
# Run from a source checkout with a built launcher and host JDK.
# CV, JAVA_HOME/JDK, and OUT override launcher, JDK, and output locations.
# Exit 3: missing prerequisite; exit 4: missing required VM dump.

set -eu

# --- locate the repository -------------------------------------------------
ROOT="$(git -C "$(dirname "$0")" rev-parse --show-toplevel 2>/dev/null || true)"
if [ -z "$ROOT" ]; then
    ROOT="$(cd "$(dirname "$0")/.." && pwd)"
fi


# --- python ----------------------------------------------------------------
# Only needed for the blocker artifacts and the self-test; the dumps themselves
# are pure VM output.
find_python() {
    if [ -n "${PYTHON:-}" ]; then
        printf '%s\n' "$PYTHON"
        return 0
    fi
    for candidate in python3 python; do
        # `command -v` is not enough on Windows: the Microsoft Store's
        # `python3` alias is on PATH, prints an advertisement and exits
        # non-zero. Probe that the interpreter actually runs.
        if command -v "$candidate" >/dev/null 2>&1 \
           && "$candidate" -c 'import sys' >/dev/null 2>&1; then
            printf '%s\n' "$candidate"
            return 0
        fi
    done
    return 1
}

# Render a path in a form the interpreter can open.
#
# This script disables MSYS/Git Bash argument conversion (see below) because
# `cratonvm` needs its arguments verbatim. A native Windows python then cannot
# open a `/c/...` MSYS path, so every path handed to python is converted
# explicitly instead of relying on the shell layer that has been turned off.
# `cygpath -m` yields `C:/...`, which both a native and an MSYS python accept.
# Outside MSYS there is no `cygpath` and this is the identity.
winpath() {
    if command -v cygpath >/dev/null 2>&1; then
        cygpath -m "$1"
    else
        printf '%s\n' "$1"
    fi
}

# --- argument handling -----------------------------------------------------
while [ "$#" -gt 0 ]; do
    case "$1" in
        -h|--help)
            # The comment header down to (but not including) `set -eu`.
            sed -n '5,/^set -eu/p' "$0" \
                | sed -e '/^set -eu$/d' -e 's/^#$//' -e 's/^# //'
            exit 0
            ;;
        *)
            echo "ERROR: unknown argument: $1" >&2
            echo "       usage: sh scripts/jdk-only-census.sh [--help]" >&2
            exit 3
            ;;
    esac
done

OUT="${OUT:-$ROOT/target/jdk-only-audit}"
mkdir -p "$OUT" "$OUT/strict"

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
# --real-jdk *requires* a real runtime image and refuses to start without one,
# so there is no useful degraded mode here: without a JDK this script would
# produce two empty censuses and a false all-clear. That is precisely the hole
# the advisory native-stub census job has today, so fail loudly instead.
JDK="${JAVA_HOME:-}"
if [ -z "$JDK" ]; then
    echo "ERROR: JAVA_HOME is not set." >&2
    echo "       --real-jdk requires a real JDK runtime image (a directory" >&2
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

# --- JDK feature version ---------------------------------------------------
# Mirrors `detect_jdk_feature` in vm-cli/src/main.rs: JAVA_VERSION out of the
# image's `release` file, falling back to `java -version`. `"25"`, `"25.0.1"`
# and `"21.0.4+7-LTS"` all yield the feature; legacy `"1.8.0_402"` yields 8.
#
# Determined here rather than read back out of a report, because a strict run
# that dies during boot may never write one — and report readers must not be left
# to guess a version that ends up in the artifact's file name.
detect_jdk_feature() {
    home="$1"
    raw=""
    if [ -f "$home/release" ]; then
        raw="$(sed -n 's/^JAVA_VERSION=//p' "$home/release" 2>/dev/null \
               | head -n 1 | tr -d '"' | tr -d '\r')"
    fi
    if [ -z "$raw" ]; then
        jbin="$home/bin/java"
        [ -x "$jbin" ] || jbin="$home/bin/java.exe"
        if [ -x "$jbin" ]; then
            raw="$("$jbin" -version 2>&1 \
                   | sed -n 's/.*version "\([^"]*\)".*/\1/p' \
                   | head -n 1 | tr -d '\r')"
        fi
    fi
    [ -n "$raw" ] || return 1
    case "$raw" in
        1.*) raw="${raw#1.}" ;;
    esac
    # Strip from the first non-digit onward: 25.0.1 -> 25, 8.0_402 -> 8.
    feature="${raw%%[!0-9]*}"
    [ -n "$feature" ] || return 1
    printf '%s\n' "$feature"
}

if [ -n "${JDK_FEATURE:-}" ]; then
    FEATURE="$JDK_FEATURE"
elif FEATURE="$(detect_jdk_feature "$JDK")"; then
    :
else
    FEATURE=""
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

# One VM invocation; all four dumps of one policy. `$@` carries only the policy
# selector, so the two runs differ in policy and in nothing else.
#
# vm-cli writes the registry / class-origin / report trio from a single
# `write_jdk_only_dumps` call on every exit path including the failing ones, and
# the grouped missing-native dump alongside it, so a run that dies still leaves
# whatever it got to. Any dump that is absent is reported below and degrades the
# derived artifacts to `partial` — it is never treated as an empty census.
run_policy() {
    label="$1"
    shift
    echo "== policy: $label =="
    rc=0
    "$CV" --java-home "$JDK" "$@" \
        --XX:AuditMissingNatives \
        --dump-native-registry         "$OUT/registry-$label.json" \
        --dump-missing-natives-grouped "$OUT/missing-$label-by-module.json" \
        --dump-class-origins           "$OUT/classes-$label.json" \
        --real-jdk-report              "$OUT/report-$label.json" \
        -cp "$PROBE_CP" "$PROBE_CLASS" \
        > "$OUT/$label.stdout.txt" 2> "$OUT/$label.stderr.txt" || rc=$?
    echo "   exit=$rc  (stdout/stderr captured in $OUT/$label.*.txt)"
    return 0
}

# 1. Permissive real-JDK run: the registry as the VM ships it today, plus the
#    missing-native work list, the class-origin census and the counter report.
#    This is the "before" half of the diff and the wave-1 measurement.
run_policy real --fast-boot

# 2. Strict run: every SyntheticStub registration refused and every
#    compatibility class refused, with the violations that resulted.
run_policy strict --real-jdk

# --- compatibility aliases -------------------------------------------------
# Byte-identical copies of the strict set under the names ci.yml reads. Removed
# first so a stale alias from an earlier run can never outlive its source.
alias_artifact() {
    src="$OUT/$1"
    dst="$OUT/$2"
    rm -f "$dst"
    if [ -s "$src" ]; then
        cp "$src" "$dst"
    fi
}

alias_artifact registry-strict.json registry-no-stubs.json
alias_artifact classes-strict.json  class-origins.json
alias_artifact report-strict.json   jdk-only-report.json

# --- verify ----------------------------------------------------------------
# Required: the four dumps the CI gates and the artifact upload depend on.
# Everything else is reported but does not fail the script — a strict run that
# dies during boot legitimately produces less, and the derived artifacts say so
# rather than pretending the missing census was empty.
missing=""
for artifact in \
    registry-real.json \
    missing-real-by-module.json \
    registry-strict.json \
    classes-strict.json \
    registry-no-stubs.json \
    class-origins.json
do
    if [ -s "$OUT/$artifact" ]; then
        echo "ok   $artifact"
    else
        echo "MISS $artifact" >&2
        missing="$missing $artifact"
    fi
done

for artifact in \
    classes-real.json \
    report-real.json \
    missing-strict-by-module.json \
    report-strict.json \
    jdk-only-report.json
do
    if [ -s "$OUT/$artifact" ]; then
        echo "ok   $artifact"
    else
        echo "note $artifact not written (optional; the run may not have reached it)"
    fi
done

if [ -n "$missing" ]; then
    echo "" >&2
    echo "ERROR: missing audit artifact(s):$missing" >&2
    echo "       See $OUT/*.stderr.txt for what the VM said." >&2
    exit 4
fi


# --- summarise -------------------------------------------------------------
# Read the `counts` block, never an occurrence count: a tag such as
# "compatibility-stub" appears once in `counts` *and* once per offending row,
# so `grep -c` reports N+1. A key that is absent from a `counts` block that was
# itself readable is a genuine zero — the emitters only write keys for kinds and
# origins that occurred. A `counts` block that could not be read at all is
# reported as unavailable, not as zero.
counts_value() {
    file="$1"
    tag="$2"
    [ -s "$file" ] || return 1
    block="$(sed -n '/"counts"[[:space:]]*:[[:space:]]*{/,/^[[:space:]]*}/p' \
             "$file" 2>/dev/null || true)"
    [ -n "$block" ] || return 1
    value="$(printf '%s\n' "$block" \
             | sed -n 's/.*"'"$tag"'"[[:space:]]*:[[:space:]]*\([0-9][0-9]*\).*/\1/p' \
             | head -n 1)"
    [ -n "$value" ] || value=0
    printf '%s\n' "$value"
}

show_count() {
    if v="$(counts_value "$1" "$2")"; then
        printf '%s' "$v"
    else
        printf 'unavailable (dump not written or has no counts block)'
    fi
}

real_stubs="$(show_count "$OUT/registry-real.json" 'synthetic-stub')"
strict_stubs="$(show_count "$OUT/registry-strict.json" 'synthetic-stub')"
real_compat="$(show_count "$OUT/classes-real.json" 'compatibility-stub')"
strict_compat="$(show_count "$OUT/classes-strict.json" 'compatibility-stub')"

echo ""
echo "== jdk-only census  (jdk feature: ${FEATURE:-unknown}) =="
echo "   policy real   — synthetic-stub natives      : $real_stubs"
echo "   policy real   — compatibility-stub classes  : $real_compat"
echo "   policy strict — synthetic-stub natives      : $strict_stubs   (target: 0)"
echo "   policy strict — compatibility-stub classes  : $strict_compat   (target: 0)"
echo "   artifacts in                                : $OUT"
echo ""
echo "   synthetic-stub rows are a classification request, not a verdict: the"
echo "   native kind is ambient (set_category / with_category, defaulting to"
echo "   SyntheticStub), so some rows are mis-tagged permanent bridges."
echo "   Classify them with docs/feature-designs/native-promotion-review.md."
