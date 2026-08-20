#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
#
# bridge-ratchet.sh — THE L6 GATE, run where a JDK exists.
#
# SCOPE, 2026-08-20 (H3-1, `G89-1` N3). This script used to take exactly ONE
# census, in `--real-jdk` (compatible) mode, while two of the five ratchets it
# scores make claims about the OTHER mode: `bridge.shadows_bytecode_anywhere` is
# contract §1.4, and `superseded.stub_lost_to_admitted` says "admitted under
# `--jdk-only`" in its own name. The strict registry ships and was measured by
# nothing.
#
# It could not have been, either: the baseline key was `<jdk-feature>/<os>` with
# the mode recorded only INSIDE the entry, so the two modes shared one slot and
# `--update-baseline` on a strict census would have overwritten the compatible
# baseline. The key is now mode-qualified for every mode but `compatible` (which
# keeps the two-part key, so every committed baseline still scores), and a
# SECOND census is taken below under `--jdk-only`.
#
# That leg is REPORTED, NOT BLOCKING, until a strict baseline is committed: with
# no `25/<os>/jdk-only` entry the gate correctly REFUSES (exit 2), and a refusal
# is not a pass. Exit 1 — the ratchet actually firing against a committed strict
# baseline — does fail. Set `BRIDGE_RATCHET_STRICT=0` to skip the leg entirely.
#
# "A gate whose stated population is wider than its measured one always reads as
# success. Nothing catches it but running the wider thing."
#
# It scores TWO gates over ONE census, and that is deliberate. The second is
# `scripts/jdk-only-kind-map.py`, a per-registration freeze of `NativeKind`
# which exists because this file's own ratchet cannot see the dangerous
# direction: flipping a `with_category` line from `Bridge` to `SyntheticStub`
# takes a registrar's worth of rows OUT of the `Bridge` population, so both
# numbers below FALL and this gate prints "IMPROVED — lock it in". That is the
# 2026-07-14 `java.util.Properties` regression reading as a win.
#
# Taking a second census for it was the obvious alternative and is the wrong
# one: two boots are two objects, and `native-builtins/tests/stub_ratchet.rs`
# already records what happens when two ratchets over "the same" VM turn out to
# be measuring different ones — they disagreed by 364 registrations for weeks.
# One census, both verdicts, and the script exits non-zero if either fires.
#
# See L6-unadjudicated-bridge-ratchet-DONE-20260805.md (the lane
# brief, retired 2026-08-05 when this landed) and
# the retired native-kind-is-ambient-and-defaults-to-syntheticstub write-up.
#
# Contract §1.5 defines a `Bridge` as what an `ACC_NATIVE` method binds to.
# 10,084 of the 10,844 `Bridge` registrations have no such target, every one of
# them inherited its kind from an ambient `set_category`, and nothing stopped
# that number rising. This boots the VM against a real JDK image, takes the
# schema-4 census, and scores it against the baseline committed in
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
#     (re-freezes BOTH baselines from the same census — they are two readings
#      of one measurement and must never be frozen from different runs)
#   CV=<cratonvm> JAVA_HOME=<jdk25> sh regression-suite/bridge-ratchet.sh
#
# Environment:
#   CV          cratonvm launcher (default: target/{release,debug}/cratonvm[.exe])
#   JAVA_HOME   real JDK runtime image (or JDK=, which run.sh also honours)
#   OUT         scratch directory (default: target/bridge-ratchet)
#   PYTHON      python interpreter (default: python3, then python)
#   JDK_FEATURE override the detected JDK feature version
#   TIMEOUT     seconds before the census run is killed (default 300)
#   BRIDGE_RATCHET_STRICT
#               1 (default) also censuses `--jdk-only` and scores it against the
#               mode-qualified baseline; 0 skips that leg
#   CRATONVM_RATCHET_ROWS
#               1 dumps the ratchet population by NAME (`@@BRIDGEROW` lines) so
#               two commits can be diffed by row identity; `all` dumps every
#               registration with its kind, which is what a suspected relabel
#               needs. Off by default — on the live registry this is thousands
#               of lines. Passed straight through to the gate.
#
# $OUT is never cleaned up on the way out: the census and the adjudication block
# are the evidence for whatever the gate just said.
#
# Exit codes — deliberately the gate script's own, passed through unchanged:
#   0  both gates passed
#   1  A GATE FIRED — an unadjudicated Bridge registration was added, or a
#      registration changed kind
#   2  refused to adjudicate (no baseline for this JDK, census not adjudicated,
#      wrong policy) — never a silent pass
#   3  a prerequisite is missing (no cratonvm binary, no JDK, no python)
set -eu

ROOT="$(git -C "$(dirname "$0")" rev-parse --show-toplevel 2>/dev/null || true)"
[ -n "$ROOT" ] || ROOT="$(cd "$(dirname "$0")/.." && pwd)"
GATE="$ROOT/scripts/jdk-only-bridge-ratchet.py"
KINDMAP="$ROOT/scripts/jdk-only-kind-map.py"

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
echo ""
"$PY" "$(winpath "$KINDMAP")" --selftest || {
    echo "ERROR: the kind-map gate's own self-test failed — see above." >&2
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

# --- the platform half of both baseline keys -------------------------------
# Derived once and passed to both gates. The bridge ratchet computes the same
# value internally; handing the kind map a different one would key two readings
# of one census to two different baselines.
case "$(uname -s 2>/dev/null || echo unknown)" in
    Linux*)  KM_OS=linux ;;
    Darwin*) KM_OS=macos ;;
    MINGW*|MSYS*|CYGWIN*|Windows*) KM_OS=windows ;;
    *)       KM_OS=unknown ;;
esac

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

# --- gate 2: the per-registration kind map ---------------------------------
# Runs even when gate 1 fired: when both have something to say, seeing both is
# what tells you whether a count moved because rows were ADDED or because
# existing rows were RE-TAGGED, and those want opposite responses.
echo ""
set -- \
    --census      "$(winpath "$OUT/census.json")" \
    --baseline-dir "$(winpath "$ROOT/scripts/baselines")" \
    --jdk-feature "$FEATURE" \
    --jdk-version "${JDK_FULL:-}" \
    --os          "$KM_OS" \
    --workload    "$WORKLOAD"
if [ "$UPDATE" -eq 1 ]; then set -- "$@" --update-baseline; fi
if [ -n "$NOTE" ]; then set -- "$@" --note "$NOTE"; fi

set +e
"$PY" "$(winpath "$KINDMAP")" "$@"
km_rc=$?
set -e

# --- the STRICT leg: the configuration that ships and was never measured -----
#
# Same binary, same probe, same gate — only `--jdk-only` differs. See the SCOPE
# note in this file's header for why it was absent and why it is reported rather
# than blocking today.
#
# `--jdk-only` composes with `--real-jdk` (they select the same class library)
# and with `--explain-jdk-only` / `--dump-native-registry`; `vm-cli`'s own
# `jdk_only_flag_surface_matches_the_contract` pins that the seven §9 flags
# parse together. Gate 2 (the kind map) is deliberately NOT run on this census:
# its committed TSVs were frozen in compatible mode and it has no mode key.
strict_fail=0
if [ "${BRIDGE_RATCHET_STRICT:-1}" = "1" ]; then
    echo ""
    echo "== census: --jdk-only against JDK ${JDK_FULL:-$FEATURE} =="
    if command -v timeout >/dev/null 2>&1; then
        set -- timeout "$TIMEOUT" "$CV"
    else
        set -- "$CV"
    fi
    src=0
    "$@" --jdk-only --java-home "$JDK" --explain-jdk-only \
         --dump-native-registry "$OUT/census.jdk-only.json" \
         -cp "$OUT/classes" "$PROBE_CLASS" \
         > "$OUT/probe.strict.stdout.txt" 2> "$OUT/probe.strict.stderr.txt" || src=$?
    echo "   vm exit=$src  (stdout/stderr in $OUT/probe.strict.*.txt)"
    if [ ! -s "$OUT/census.jdk-only.json" ]; then
        # NOT a hard failure, and the reason is honesty rather than leniency:
        # this leg has never been run in CI, so its first landing must not be
        # able to turn a green build red for a reason nobody has diagnosed. It
        # is loud instead, and promoting it is deleting this branch.
        echo "   NOTE: no strict census was written (vm exit $src). The --jdk-only"
        echo "         registry is therefore UNMEASURED on this run — that is a gap,"
        echo "         not a pass. See $OUT/probe.strict.stderr.txt."
    else
        set -- \
            --census      "$(winpath "$OUT/census.jdk-only.json")" \
            --baseline    "$(winpath "$ROOT/scripts/baselines/jdk-only-bridge-ratchet.json")" \
            --jdk-feature "$FEATURE" \
            --jdk-version "${JDK_FULL:-}" \
            --workload    "$WORKLOAD"
        if [ "$UPDATE" -eq 1 ]; then set -- "$@" --update-baseline; fi
        if [ -n "$NOTE" ]; then set -- "$@" --note "$NOTE (--jdk-only leg)"; fi
        set +e
        "$PY" "$(winpath "$GATE")" "$@"
        strict_rc=$?
        set -e
        case "$strict_rc" in
            0) echo "   strict leg: pass." ;;
            2) echo "   NOTE: no committed baseline for the --jdk-only leg yet, so it"
               echo "         REFUSED (exit 2). A refusal is not a pass: the strict"
               echo "         registry is unmeasured until someone runs"
               echo "         'sh regression-suite/bridge-ratchet.sh --update-baseline"
               echo "         --note \"...\"' on this image and commits the result." ;;
            1) echo "   STRICT BRIDGE-RATCHET FIRED — see above."; strict_fail=1 ;;
            *) echo "   NOTE: the strict gate exited $strict_rc; nothing was scored." ;;
        esac
    fi
fi

echo ""
echo "   census        : $OUT/census.json"
echo "   adjudication  : $OUT/adjudication.json"
if [ "${BRIDGE_RATCHET_STRICT:-1}" = "1" ] && [ -s "$OUT/census.jdk-only.json" ]; then
    echo "   strict census : $OUT/census.jdk-only.json"
fi
if [ "$gate_rc" -ne 0 ]; then exit "$gate_rc"; fi
if [ "$km_rc" -ne 0 ]; then exit "$km_rc"; fi
exit "$strict_fail"
