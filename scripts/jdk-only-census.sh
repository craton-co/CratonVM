#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
#
# jdk-only-census.sh — produce the JDK-only mode audit artifacts.
#
# See docs/feature-designs/jdk-only-mode.md (the normative contract) and
# jdk-only-audit.md §3 and §6 (the reproducible audit).
#
# ---------------------------------------------------------------------------
# ONE INVOCATION PER POLICY. ALL FOUR DUMPS PER INVOCATION.
# ---------------------------------------------------------------------------
#
# The four dumps describe *one booted VM*. A registry census, a class-origin
# census, a missing-native list and a violation report are only comparable when
# they came from the same process under the same compatibility policy. Pairing
# a permissive registry with a strict class census — or vice versa — silently
# combines two different worlds, and any blocker list built from that mix is
# wrong in a way that looks fine.
#
# So each policy gets exactly one VM run that emits all four dumps, and every
# dump file carries its policy in its name. There is no unsuffixed dump for a
# consumer to pick up by accident.
#
#   policy `real`   (--real-jdk, CompatibilityMode::Compatible — today's VM)
#       registry-real.json              native registry census (schema 2)
#       missing-real-by-module.json     unresolved ACC_NATIVE, by JDK module
#       classes-real.json               class-origin census
#       report-real.json                violation/counter report
#
#   policy `strict` (--jdk-only, CompatibilityMode::JdkOnly)
#       registry-strict.json
#       missing-strict-by-module.json
#       classes-strict.json
#       report-strict.json
#
# Three flat aliases are also written, byte-identical copies of the *strict*
# set, because `.github/workflows/ci.yml` reads them by name and this script
# does not own that file:
#
#       registry-no-stubs.json  <- registry-strict.json
#       class-origins.json      <- classes-strict.json
#       jdk-only-report.json    <- report-strict.json
#
# All three alias the same policy, so the alias set is coherent too. They exist
# for the CI gates and the artifact upload; new consumers should read the
# policy-suffixed names.
#
# RENAMED, and why: what this script used to call `class-origins.json` and
# `jdk-only-report.json` are now `classes-{real,strict}.json` and
# `report-{real,strict}.json`. The old names said nothing about which policy
# produced them, which is exactly how they came to be paired with the
# permissive `registry-real.json`. `registry-strict.json` is the strict registry
# under the name jdk-only-audit.md §3.2 uses for it; `registry-no-stubs.json`
# is kept as an alias because CI's zero-stub gate reads that path.
#
# ---------------------------------------------------------------------------
# Derived artifacts (jdk-only-audit.md §6)
# ---------------------------------------------------------------------------
#
# tools/jdk-only-blockers/blockers.py turns one *coherent* dump set into:
#
#       jdk-<feature>-missing-natives.json
#       jdk-<feature>-synthetic-dependencies.json
#
#   $OUT/         <- generated from the `real` dump set (the wave-1 measure of
#                    how far --jdk-only still has to go; §6's canonical path)
#   $OUT/strict/  <- generated from the `strict` dump set
#
# The JDK feature version is passed explicitly, read from $JAVA_HOME/release.
# It is never inferred from a report that a failing run may not have written:
# the version is part of the file name, and an artifact keyed to the wrong
# image is worse than no artifact.
#
# `synthetic-stub` rows in those files are flagged as *requiring
# classification*, not asserted as defects — the native kind is ambient
# (`set_category` / `with_category`, defaulting to `SyntheticStub`), so some
# rows are genuinely mis-tagged permanent bridges. See
# docs/jdk-only-native-review.md.
#
# POSIX sh on purpose (no bashisms): this runs from CI on ubuntu-latest and
# windows-latest, and from Git Bash / MSYS on a developer machine.
#
# Usage:
#   sh scripts/jdk-only-census.sh
#   sh scripts/jdk-only-census.sh --selftest   # hermetic; no VM, no JDK
#   CV=/path/to/cratonvm JAVA_HOME=/path/to/jdk sh scripts/jdk-only-census.sh
#   OUT=/tmp/audit sh scripts/jdk-only-census.sh
#   BLOCKERS=check sh scripts/jdk-only-census.sh
#
# Environment:
#   CV          cratonvm launcher (default: target/{release,debug}/cratonvm)
#   JAVA_HOME   real JDK runtime image (required)
#   OUT         output directory (default: target/jdk-only-audit)
#   PROBE_CP    census a real application instead of the built-in probe
#   PROBE_CLASS main class for PROBE_CP
#   PYTHON      python interpreter (default: python3, then python)
#   JDK_FEATURE override the detected JDK feature version (e.g. 25)
#   BLOCKERS    generate (default) | check | update | off
#                 generate  write the blocker artifacts
#                 check     ...and ratchet the `real` pair against the
#                           committed baseline (fails if the open set grew)
#                 update    ...and refresh the committed `real` baseline
#                 off       skip blocker generation entirely
#
# Exit codes:
#   0  every artifact was produced
#   3  a prerequisite is missing (no cratonvm binary, no JDK) — nothing ran
#   4  the VM ran but at least one required dump was not written
#   5  the blocker artifacts could not be generated, or the ratchet failed
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

BLOCKER_DIR="$ROOT/tools/jdk-only-blockers"

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
# `--selftest` is the hermetic entry point: stdlib-only, no VM build, no JDK,
# no network. It is deliberately handled before every prerequisite check so a
# lint job can run it without provisioning anything.
SELFTEST_ONLY=0
while [ "$#" -gt 0 ]; do
    case "$1" in
        --selftest)
            SELFTEST_ONLY=1
            shift
            ;;
        -h|--help)
            # The comment header down to (but not including) `set -eu`.
            sed -n '5,/^set -eu/p' "$0" \
                | sed -e '/^set -eu$/d' -e 's/^#$//' -e 's/^# //'
            exit 0
            ;;
        *)
            echo "ERROR: unknown argument: $1" >&2
            echo "       usage: sh scripts/jdk-only-census.sh [--selftest]" >&2
            exit 3
            ;;
    esac
done

if [ "$SELFTEST_ONLY" -eq 1 ]; then
    if PY="$(find_python)"; then :; else
        echo "ERROR: no python interpreter found (tried \$PYTHON, python3, python)." >&2
        echo "       tools/jdk-only-blockers/selftest.py needs one; it needs nothing else." >&2
        exit 3
    fi
    echo "== jdk-only-blockers self-test ($PY) =="
    exec "$PY" "$(winpath "$BLOCKER_DIR/selftest.py")"
fi

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

# --- JDK feature version ---------------------------------------------------
# Mirrors `detect_jdk_feature` in vm-cli/src/main.rs: JAVA_VERSION out of the
# image's `release` file, falling back to `java -version`. `"25"`, `"25.0.1"`
# and `"21.0.4+7-LTS"` all yield the feature; legacy `"1.8.0_402"` yields 8.
#
# Determined here rather than read back out of a report, because a strict run
# that dies during boot may never write one — and blockers.py must not be left
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
        --jdk-only-report              "$OUT/report-$label.json" \
        -cp "$PROBE_CP" "$PROBE_CLASS" \
        > "$OUT/$label.stdout.txt" 2> "$OUT/$label.stderr.txt" || rc=$?
    echo "   exit=$rc  (stdout/stderr captured in $OUT/$label.*.txt)"
    return 0
}

# 1. Permissive real-JDK run: the registry as the VM ships it today, plus the
#    missing-native work list, the class-origin census and the counter report.
#    This is the "before" half of the diff and the wave-1 measurement.
run_policy real --real-jdk

# 2. Strict run: every SyntheticStub registration refused and every
#    compatibility class refused, with the violations that resulted.
run_policy strict --jdk-only

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

# --- derived blocker artifacts (jdk-only-audit.md §6) -----------------
BLOCKERS="${BLOCKERS:-generate}"
blocker_status=0

case "$BLOCKERS" in
    off)
        echo ""
        echo "note BLOCKERS=off — jdk-<feature>-*.json were not generated."
        ;;
    generate|check|update)
        echo ""
        if PY="$(find_python)"; then :; else
            echo "ERROR: no python interpreter found (tried \$PYTHON, python3, python)." >&2
            echo "       The dumps above were written, but the per-JDK blocker artifacts" >&2
            echo "       required by jdk-only-audit.md §6 were NOT generated." >&2
            echo "       That is a gap in this run, not a clean result. Install python or" >&2
            echo "       set BLOCKERS=off to declare the omission deliberate." >&2
            exit 5
        fi
        if [ -z "$FEATURE" ]; then
            echo "ERROR: could not determine the JDK feature version from $JDK." >&2
            echo "       ($JDK/release has no JAVA_VERSION, and 'java -version' did not" >&2
            echo "       yield one.) The version is part of the artifact file name, so" >&2
            echo "       guessing it would produce a file keyed to the wrong image." >&2
            echo "       Set JDK_FEATURE=<n> explicitly, or BLOCKERS=off." >&2
            exit 5
        fi

        # `real` is the canonical §6 pair: wave 1 is measurement, not deletion,
        # so the Compatible run's census is the measure of how far --jdk-only
        # still has to go. It is also the only pair the committed baseline
        # covers, so it is the only one that may be ratcheted.
        BLOCKERS_PY="$(winpath "$BLOCKER_DIR/blockers.py")"
        set -- \
            --jdk-feature             "$FEATURE" \
            --repo-root               "$(winpath "$ROOT")" \
            --status-ledger           "$(winpath "$BLOCKER_DIR/status-ledger.json")" \
            --baseline-dir            "$(winpath "$BLOCKER_DIR/baselines")" \
            --native-registry         "$(winpath "$OUT/registry-real.json")" \
            --missing-natives-grouped "$(winpath "$OUT/missing-real-by-module.json")" \
            --class-origins           "$(winpath "$OUT/classes-real.json")" \
            --jdk-only-report         "$(winpath "$OUT/report-real.json")" \
            --out-dir                 "$(winpath "$OUT")"
        case "$BLOCKERS" in
            check)  set -- "$@" --check ;;
            update) set -- "$@" --update-baseline ;;
        esac
        echo "== blockers: real (jdk $FEATURE) =="
        "$PY" "$BLOCKERS_PY" "$@" || blocker_status=$?

        # The strict pair is generated but never ratcheted: `baselines/` holds
        # one pair per JDK feature version, and ratcheting a second policy
        # against it would compare two different worlds — the defect this
        # script exists to not repeat.
        strict_status=0
        echo "== blockers: strict (jdk $FEATURE) =="
        "$PY" "$BLOCKERS_PY" \
            --jdk-feature             "$FEATURE" \
            --repo-root               "$(winpath "$ROOT")" \
            --status-ledger           "$(winpath "$BLOCKER_DIR/status-ledger.json")" \
            --native-registry         "$(winpath "$OUT/registry-strict.json")" \
            --missing-natives-grouped "$(winpath "$OUT/missing-strict-by-module.json")" \
            --class-origins           "$(winpath "$OUT/classes-strict.json")" \
            --jdk-only-report         "$(winpath "$OUT/report-strict.json")" \
            --out-dir                 "$(winpath "$OUT/strict")" \
            || strict_status=$?
        if [ "$strict_status" -ne 0 ] && [ "$blocker_status" -eq 0 ]; then
            blocker_status="$strict_status"
        fi
        ;;
    *)
        echo "ERROR: BLOCKERS=$BLOCKERS is not one of: generate check update off" >&2
        exit 5
        ;;
esac

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
if [ "$BLOCKERS" != "off" ]; then
    echo "   blockers (real)                             : $OUT/jdk-$FEATURE-*.json"
    echo "   blockers (strict)                           : $OUT/strict/jdk-$FEATURE-*.json"
fi
echo ""
echo "   synthetic-stub rows are a classification request, not a verdict: the"
echo "   native kind is ambient (set_category / with_category, defaulting to"
echo "   SyntheticStub), so some rows are mis-tagged permanent bridges."
echo "   Classify them with docs/jdk-only-native-review.md."

# blockers.py's own codes (1 ratchet grew, 2 configuration, 3 partial where a
# complete result was required) are collapsed onto this script's 5 and named,
# so a caller never has to guess which tool produced a bare exit status.
if [ "$blocker_status" -ne 0 ]; then
    echo "" >&2
    echo "ERROR: blockers.py exited $blocker_status" >&2
    case "$blocker_status" in
        1) echo "       the open blocker set grew against the committed baseline." >&2
           echo "       An entry that was never invoked is still a blocker." >&2 ;;
        2) echo "       usage / configuration error (see the message above)." >&2 ;;
        3) echo "       a partial result where a complete one was required." >&2 ;;
        *) echo "       see the message above." >&2 ;;
    esac
    exit 5
fi
