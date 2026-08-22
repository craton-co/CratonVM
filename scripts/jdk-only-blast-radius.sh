#!/usr/bin/env bash
# jdk-only-blast-radius.sh — price a native retirement BEFORE anyone attempts it.
#
# `CRATONVM_ENFORCE_NATIVE_SHADOW=<prefix>` makes contract §1.4 ENFORCED rather
# than counted for that prefix: the native stops winning and real JDK bytecode
# runs. That is the same thing a permanent retirement does, so an armed run of
# the existing strict arm measures a retirement's blast radius for ONE ENV VAR
# and NO BUILD. It is the only instrument in the tree that costs less than the
# work it prices.
#
# **That premise became true on 2026-08-21 and was false before it** -- the dial
# reached one of fourteen dispatch doors, so "the native stops winning" applied
# to a class's cold step-1 dispatches and nothing else. Caveat 4 below has the
# measurement and the direction of the error. A cell from an older binary is not
# a conservative version of a current cell; it is a different measurement.
#
# `H0-4` swept six collection prefixes by hand on 2026-08-20 and found a 23x
# spread where every previous argument had treated "the collection cluster" as
# one body. That sweep exists because somebody remembered to type it. This
# script is `H0-4` N3: the same six runs, reproducible, with a table at the end.
#
#   bash scripts/jdk-only-blast-radius.sh
#   bash scripts/jdk-only-blast-radius.sh --prefixes 'java/util/HashSet java/util/Hashtable'
#   bash scripts/jdk-only-blast-radius.sh --update-baseline --note 'first linux sweep'
#
# ---------------------------------------------------------------------------
# THE THREE THINGS THIS INSTRUMENT WILL LIE TO YOU ABOUT
# ---------------------------------------------------------------------------
#
#  1. **A green cell is not a clean family.** The corpus is ~105 vectors and has
#     no AWT vector at all (`G79-1`). `HashSet` at 103/104 in `H0-4` means "one
#     vector in this corpus objects", not "`HashSet` is retirable". A zero is
#     permission to attempt the migration and measure it — never permission to
#     skip measuring it. `G90-1` §5 is the standing instance of a screen passing
#     what the wider arm rejected.
#
#  2. **The cells DO NOT ADD UP, and this script deliberately prints no total.**
#     Each prefix is measured ALONE. `H0-4` §5: "their sum is not the cost of
#     arming all six, and nobody has run that combination." Arming two prefixes
#     at once is a different measurement and this script will happily take it
#     (`--prefixes` accepts a `+`-joined group, see below) — but it must be
#     RUN, not derived.
#
#  3. **The prefixes are not disjoint in effect.** `java/util/LinkedHashMap`
#     extends `java/util/HashMap`, and `java/util/HashSet` is backed by a
#     `HashMap`. Arming `HashMap` already enforces shadows on classes the
#     `LinkedHashMap` run also touches. Two rows overlapping is not a
#     contradiction in the table; it is the class hierarchy.
#
#  4. **Every cell taken before 2026-08-21 is void, and NOT in the direction
#     its own caveat claimed.** `CRATONVM_ENFORCE_NATIVE_SHADOW` used to be read
#     at exactly ONE dispatch site (`resolve_step1_native`); every other door
#     computed `bytecode_available` without it. MEASURED: 890 of 947 armed
#     `Bridge` dispatches never asked the dial, 299 431 of 299 469 on a hot
#     workload. That is fixed -- all fourteen doors consult it now, and three
#     source-witness tests in `native_override.rs` keep them consulting it.
#
#     The caveat this replaces said "the number is a floor", i.e. that a real
#     retirement would hurt at least as much. **That was backwards.**
#     `java/util/HashMap` scored **81/104 half-armed and 86/104 fully armed**:
#     the old cells overstated the damage. Half-armed is not a partial
#     retirement, it is a state no configuration can otherwise reach -- bytecode
#     built the table and a native then wrote into it -- and the witness was a
#     `HashMap` reporting `size()==4` over four occupied buckets while iterating
#     one key. Uniform-native works and uniform-bytecode works; the MIXTURE is
#     corrupt.
#
#     One conservatism does survive, and this one really is a floor: a yield
#     needs concrete bytecode to yield TO, so a triple whose shadowed method has
#     no `Code` still runs its native. The report's
#     `enforcement_dial.declined_no_bytecode` counts exactly those (0 armed for
#     `java/util/HashMap`; 494 of 19 931 armed for `all`).
#
#  5. **Four of the six witnesses this directory reaches for are BLIND**, so
#     do not spot-check a cell with one at random -- MEASURED, `H17-2` §2:
#
#       bucket head class    NO  -- the native mints a real `HashMap$Node`
#       `modCount`           NO  -- the native maintains it
#       key `hashCode()`     NO  -- the native calls it once per put, as bytecode does
#       `equals()` count     NO  -- zero in every configuration
#       `table` ARRAY class  YES -- but ONE BIT PER MAP (did that map's FIRST
#                                  insert run bytecode), never a count. Reading
#                                  it as a count is the shared error of
#                                  `H16-3`, `H0-8` and `H17-1`.
#       `--jdk-only-report`  presence only, never a count
#
#     `regression-suite/probes/DialWitness.java` is the surviving witness plus a
#     consistency check, one case per process.
#
# ---------------------------------------------------------------------------
# WHY THE SIGNAL IS THE FAILING SET AND NOT THE PASS COUNT
# ---------------------------------------------------------------------------
#
# A pass count moves whenever the corpus grows. `regression-suite/src` gained
# two vectors on 2026-08-19 and a third on 2026-08-20; every published
# denominator in `docs/known-issues/jdk-only/` is therefore already stale.
# A gate keyed on `103` would have gone red for a reason that is not a
# regression, which is `G89-1`'s species: a gate that is red for a reason nobody
# can act on adjudicates nothing, and people learn to scroll past it.
#
# So the baseline stores, per prefix, the SET of vectors that failed, plus the
# scheduled corpus at the time it was taken. A difference is classified:
#
#     REGRESSION   a vector that passed under this prefix and now fails
#     REPAIRED     a vector that failed under this prefix and now passes
#     NEW-VECTOR   a failing vector that did not exist when the baseline was
#                  taken — the corpus grew, this is not a regression, and it
#                  needs adjudicating rather than baselining away
#     RETIRED      a baselined vector that is no longer scheduled
#
# and the pass counts are printed as evidence beside the sets rather than gated
# on.
#
# ---------------------------------------------------------------------------
# EXIT CODES — and read the CI note before wiring this as a blocking gate
# ---------------------------------------------------------------------------
#
#   0  every armed cell's failing set matches the baseline (or matches with
#      only NEW-VECTOR / RETIRED differences, which the report names)
#   1  at least one cell MOVED: a REGRESSION or a REPAIRED row
#   2  no ADJUDICATING baseline for this key. The table is still printed. This
#      is NOT a pass — a baseline taken on another key cannot score this one
#   3  a prerequisite is missing, or an arm did not complete (so nothing was
#      measured)
#
# **These cells are EXPECTED TO BE RED and this must not be a blocking gate.**
# `G89-1` measured a ratchet that had been red in blocking CI for five days and
# "adjudicated nothing"; `H3-1` found a gate in the same CI that had not
# COMPILED since a merge. A gate whose red state is the normal state trains
# every lane to ignore it, and then it cannot report the abnormal one. Exit 1 is
# for a human or a scheduled job that turns it into a notice — see
# `.github/workflows/jdk-only-blast-radius.yml`, which is `schedule:` +
# `workflow_dispatch:` and `continue-on-error`.
#
# ---------------------------------------------------------------------------
# WHY THE ARMS RUN SEQUENTIALLY
# ---------------------------------------------------------------------------
#
# `regression-suite/run.sh` uses a FIXED `.guard-tmp` directory for the oracle
# transcripts. Two concurrent runs destroy each other's oracle files and produce
# well-formed results about the wrong thing (`W8-D2-1`). Seven arms, one after
# another, is the only safe schedule.
set -u

usage() {
    cat <<'USAGE'
usage: bash scripts/jdk-only-blast-radius.sh [options]

  --prefixes "<p1> <p2> ..."   Prefixes to arm, one arm each. Default: the six
                               collection families H0-4 measured. A prefix may
                               be a '+'-joined GROUP ("java/util/HashSet+java/util/Hashtable")
                               to arm several at once in one arm — H0-4 N2.
  --update-baseline            Write the measured sets to the baseline for this
                               key and exit 0. Never combine with a run whose
                               arms you have not read.
  --note "<text>"              Provenance note recorded in the baseline.
  --baseline-key <key>         Override the auto-derived <feature>-<os> key.
  --keep-logs                  Do not delete the per-arm run.sh transcripts.
  --skip-control               Do not run the UNARMED control arm. Only for
                               re-measuring one cell; the table then cannot
                               separate "the dial did this" from "this vector
                               was already red".
  -h, --help                   This text.

env: CV=<cratonvm binary>  JDK=<jdk home>  TIMEOUT=<seconds, per vector>
USAGE
}

PREFIXES_DEFAULT="java/util/HashSet java/util/Hashtable java/util/LinkedHashMap java/util/TreeMap java/util/concurrent/ConcurrentHashMap java/util/HashMap"
PREFIXES="$PREFIXES_DEFAULT"
UPDATE=""
NOTE=""
KEY_OVERRIDE=""
KEEP_LOGS=""
SKIP_CONTROL=""

while [ $# -gt 0 ]; do
    case "$1" in
        --prefixes)        PREFIXES="${2:-}"; shift 2 ;;
        --update-baseline) UPDATE=1; shift ;;
        --note)            NOTE="${2:-}"; shift 2 ;;
        --baseline-key)    KEY_OVERRIDE="${2:-}"; shift 2 ;;
        --keep-logs)       KEEP_LOGS=1; shift ;;
        --skip-control)    SKIP_CONTROL=1; shift ;;
        -h|--help)         usage; exit 0 ;;
        *) echo "ERROR: unknown option '$1'" >&2; usage >&2; exit 3 ;;
    esac
done

# ROOT must FAIL LOUDLY rather than fall back — same reasoning, and the same
# incident, as run.sh's own header: a fallback on this case-insensitive
# filesystem silently measures the MAIN CHECKOUT and reports well-formed results
# about a tree you are not in.
ROOT="$(git -C "$(dirname "${BASH_SOURCE[0]}")" rev-parse --show-toplevel 2>/dev/null)"
if [ -z "$ROOT" ] || [ ! -f "$ROOT/regression-suite/run.sh" ]; then
    echo "ERROR: cannot resolve the repository root from $(dirname "${BASH_SOURCE[0]}")." >&2
    echo "       Run this script from inside its own worktree; do not copy it elsewhere." >&2
    exit 3
fi

# RELEASES= makes run.sh print one COUNTS: line per level, and this script's
# parse would silently read only the last. Refuse rather than mis-measure.
if [ -n "${RELEASES:-}" ]; then
    echo "ERROR: RELEASES is set. Each level is a separate corpus reading and this" >&2
    echo "       script keys one baseline per arm. Unset it and run the levels apart." >&2
    exit 3
fi
# Likewise SUITE and ONLY: the baseline records the scheduled corpus, and a
# baseline taken over a subset would score a full run as 40 REPAIRED rows.
if [ -n "${SUITE:-}" ] || [ -n "${ONLY:-}" ]; then
    echo "ERROR: SUITE or ONLY is set. --jdk-only implies the full strict schedule and" >&2
    echo "       the baseline records it; a subset run cannot score against it." >&2
    exit 3
fi
if [ -n "${CRATONVM_ENFORCE_NATIVE_SHADOW:-}" ]; then
    echo "ERROR: CRATONVM_ENFORCE_NATIVE_SHADOW is already set in the environment." >&2
    echo "       This script sets it per arm; an inherited value would arm every arm." >&2
    exit 3
fi

CV="${CV:-$ROOT/target/release/cratonvm}"
[ -x "$CV" ] || CV="$ROOT/target/release/cratonvm.exe"
if [ ! -x "$CV" ]; then
    echo "ERROR: CratonVM binary not found (tried $ROOT/target/release/cratonvm[.exe])." >&2
    echo "       Set CV=<path>. This gate needs a BUILT VM; it prices a retirement," >&2
    echo "       it does not read source." >&2
    exit 3
fi
JDK="${JDK:-${JAVA_HOME:-}}"
if [ -z "$JDK" ] || [ ! -d "$JDK" ]; then
    echo "ERROR: no JDK. Set JDK=<home> or JAVA_HOME=<home>." >&2
    exit 3
fi

# --- the baseline key ------------------------------------------------------
# Mirrors regression-suite/bridge-ratchet.sh so the two gates cannot key one
# reading to two different baselines.
read_jdk_version() {
    [ -f "$1/release" ] && sed -n 's/^JAVA_VERSION=//p' "$1/release" 2>/dev/null \
        | head -n 1 | tr -d '"' | tr -d '\r'
}
detect_jdk_feature() {
    raw="$(read_jdk_version "$1")"
    [ -n "$raw" ] || return 1
    case "$raw" in 1.*) raw="${raw#1.}" ;; esac
    feature="${raw%%[!0-9]*}"
    [ -n "$feature" ] || return 1
    printf '%s\n' "$feature"
}
case "$(uname -s 2>/dev/null || echo unknown)" in
    Linux*)  BR_OS=linux ;;
    Darwin*) BR_OS=macos ;;
    MINGW*|MSYS*|CYGWIN*|Windows*) BR_OS=windows ;;
    *)       BR_OS=unknown ;;
esac
if [ -n "$KEY_OVERRIDE" ]; then
    KEY="$KEY_OVERRIDE"
elif FEATURE="$(detect_jdk_feature "$JDK")"; then
    KEY="$FEATURE-$BR_OS"
else
    echo "ERROR: could not read JAVA_VERSION from $JDK/release. The baseline is keyed" >&2
    echo "       by feature version; guessing it would score against the wrong image." >&2
    exit 3
fi
BASELINE="$ROOT/scripts/baselines/jdk-only-blast-radius-$KEY.txt"

OUTDIR="$ROOT/regression-suite/.blast-radius.$$"
rm -rf "$OUTDIR"; mkdir -p "$OUTDIR" || { echo "ERROR: cannot create $OUTDIR" >&2; exit 3; }
cleanup() { [ -n "$KEEP_LOGS" ] || rm -rf "$OUTDIR"; }
trap cleanup EXIT

# --- one arm ---------------------------------------------------------------
#
# ARM_PASSED / ARM_TOTAL come from run.sh's own COUNTS: line, which is the ONLY
# place it prints a pair that shares a denominator. Its "N passed, M failed"
# line sums four incommensurable populations and must never be read as a pair —
# run.sh says so itself, at length.
ARM_PASSED=""; ARM_TOTAL=""; ARM_FAILED=""; ARM_SCHEDULED=""
run_arm() {
    arm_label="$1"; arm_prefix="$2"
    arm_log="$OUTDIR/$arm_label.log"
    echo "-- arm: $arm_label${arm_prefix:+  (CRATONVM_ENFORCE_NATIVE_SHADOW=$arm_prefix)}" >&2
    if [ -n "$arm_prefix" ]; then
        CRATONVM_ENFORCE_NATIVE_SHADOW="$arm_prefix" \
        CRATONVM_ARGS="--jdk-only" CV="$CV" JDK="$JDK" \
            bash "$ROOT/regression-suite/run.sh" > "$arm_log" 2>&1
    else
        CRATONVM_ARGS="--jdk-only" CV="$CV" JDK="$JDK" \
            bash "$ROOT/regression-suite/run.sh" > "$arm_log" 2>&1
    fi
    # rc is DELIBERATELY IGNORED: an armed arm is expected to be non-zero, that
    # is the whole measurement. Completion is judged on the COUNTS: line, which
    # run.sh prints unconditionally at the end and which a killed run does not
    # reach.
    ARM_PASSED="$(sed -n 's/^  COUNTS: \([0-9][0-9]*\) of \([0-9][0-9]*\) SCHEDULED.*/\1/p' "$arm_log" | tail -1)"
    ARM_TOTAL="$( sed -n 's/^  COUNTS: \([0-9][0-9]*\) of \([0-9][0-9]*\) SCHEDULED.*/\2/p' "$arm_log" | tail -1)"
    if [ -z "$ARM_PASSED" ] || [ -z "$ARM_TOTAL" ]; then
        echo "ERROR: arm '$arm_label' printed no COUNTS: line — it did not complete, so" >&2
        echo "       NOTHING was measured. Transcript: $arm_log (re-run with --keep-logs)." >&2
        KEEP_LOGS=1
        return 1
    fi
    ARM_FAILED="$(sed -n 's/^  \([A-Za-z_][A-Za-z0-9_]*\)  *FAIL .*/\1/p' "$arm_log" | sort -u | tr '\n' ' ')"
    ARM_FAILED="${ARM_FAILED% }"
    ARM_SCHEDULED="$( { sed -n 's/^  \([A-Za-z_][A-Za-z0-9_]*\)  *PASS *$/\1/p' "$arm_log"
                        sed -n 's/^  \([A-Za-z_][A-Za-z0-9_]*\)  *FAIL .*/\1/p' "$arm_log"; } | sort -u | tr '\n' ' ')"
    ARM_SCHEDULED="${ARM_SCHEDULED% }"
    return 0
}

in_set() { case " $2 " in *" $1 "*) return 0 ;; esac; return 1; }

# --- quarantine ------------------------------------------------------------
#
# Vectors MEASURED to be non-deterministic, from regression-suite/known-flaky.txt.
# They are dropped from every cell below and REPORTED separately, never silently.
# See that file's header for why a rate and a record are required, and
# `WORKER-1-NOTE-1` for the measurement behind the row that is there today.
QUARANTINE_FILE="$ROOT/regression-suite/known-flaky.txt"
QUARANTINED=""
if [ -f "$QUARANTINE_FILE" ]; then
    QUARANTINED="$(sed 's/#.*//' "$QUARANTINE_FILE" | awk 'NF {print $1}' | sort -u | tr '\n' ' ')"
    QUARANTINED="${QUARANTINED% }"
fi

# One "<arm><TAB><vector><TAB>FAIL|pass|absent" per line, for the report below.
QLOG=""
note_quarantine() {   # $1 = arm label, $2 = that arm's failing set, $3 = scheduled
    for qv in $QUARANTINED; do
        if in_set "$qv" "$2"; then         QLOG="$QLOG$1	$qv	FAIL
"
        elif in_set "$qv" "$3"; then       QLOG="$QLOG$1	$qv	pass
"
        else                               QLOG="$QLOG$1	$qv	absent
"
        fi
    done
}

# --- the control -----------------------------------------------------------
CONTROL_FAILED=""; CONTROL_PASSED="?"; CONTROL_TOTAL="?"; CONTROL_SCHEDULED=""
if [ -z "$SKIP_CONTROL" ]; then
    run_arm control "" || exit 3
    CONTROL_FAILED="$ARM_FAILED"
    CONTROL_PASSED="$ARM_PASSED"; CONTROL_TOTAL="$ARM_TOTAL"
    CONTROL_SCHEDULED="$ARM_SCHEDULED"
    note_quarantine control "$ARM_FAILED" "$ARM_SCHEDULED"
fi

# A quarantine row naming a vector this corpus does not schedule is a stale
# excuse, and a stale excuse reads as coverage. Same rule the exemption list in
# `native_override.rs` obeys: the list must be able to fail.
if [ -n "$QUARANTINED" ] && [ -n "$CONTROL_SCHEDULED" ]; then
    for qv in $QUARANTINED; do
        if ! in_set "$qv" "$CONTROL_SCHEDULED"; then
            echo "ERROR: regression-suite/known-flaky.txt quarantines '$qv', which this" >&2
            echo "       corpus does not schedule. Drop the row — a quarantine entry that" >&2
            echo "       matches nothing silently grows the list and excuses nothing." >&2
            exit 3
        fi
    done
fi

# --- the armed sweep -------------------------------------------------------
ROWS=""             # one "prefix<TAB>passed<TAB>total<TAB>failed-set" per line
COMMON=""; COMMON_INIT=""
for p in $PREFIXES; do
    label="$(printf '%s' "$p" | tr '/+' '__')"
    run_arm "$label" "$p" || exit 3
    # Net of the control: a vector already red WITHOUT the dial is not this
    # family's cost. H0-4 §4 found four of its six cells inflated by one shared
    # row and re-priced the whole table on it.
    net=""
    for v in $ARM_FAILED; do
        in_set "$v" "$CONTROL_FAILED" && continue
        # Quarantined: MEASURED flaky, so its presence here is as likely to be
        # the coin as the prefix. Excluded from the cell, reported below.
        in_set "$v" "$QUARANTINED" && continue
        net="$net $v"
    done
    net="${net# }"
    note_quarantine "$label" "$ARM_FAILED" "$ARM_SCHEDULED"
    ROWS="$ROWS$p	$ARM_PASSED	$ARM_TOTAL	$net
"
    if [ -z "$COMMON_INIT" ]; then COMMON="$net"; COMMON_INIT=1
    else
        keep=""
        for v in $COMMON; do in_set "$v" "$net" && keep="$keep $v"; done
        COMMON="${keep# }"
    fi
    [ -z "$ARM_SCHEDULED" ] || CONTROL_SCHEDULED="${CONTROL_SCHEDULED:-$ARM_SCHEDULED}"
done

# --- --update-baseline -----------------------------------------------------
if [ -n "$UPDATE" ]; then
    mkdir -p "$(dirname "$BASELINE")"
    {
        echo "# jdk-only blast radius — baseline for key '$KEY'"
        echo "#"
        echo "# Written by scripts/jdk-only-blast-radius.sh --update-baseline."
        echo "# The SIGNAL is the failing SET per prefix, not the pass count: the count"
        echo "# moves whenever the corpus grows. See the script header."
        echo "#"
        echo "# Lines: !key !adjudicating !taken !note !corpus, then one row per prefix:"
        echo "#   <prefix><TAB><passed><TAB><total><TAB><failing vectors, net of control>"
        echo "!key	$KEY"
        echo "!adjudicating	yes"
        echo "!taken	$(date -u '+%Y-%m-%dT%H:%M:%SZ')	rev=$(git -C "$ROOT" rev-parse --short HEAD 2>/dev/null || echo '?')"
        echo "!note	${NOTE:-(none)}"
        echo "!control	$CONTROL_PASSED	$CONTROL_TOTAL	$CONTROL_FAILED"
        echo "!corpus	$CONTROL_SCHEDULED"
        echo "!quarantined	$QUARANTINED"
        printf '%s' "$ROWS"
    } > "$BASELINE"
    echo "baseline written: $BASELINE"
    exit 0
fi

# --- the table -------------------------------------------------------------
echo
echo "JDK-ONLY BLAST RADIUS — key $KEY, rev $(git -C "$ROOT" rev-parse --short HEAD 2>/dev/null || echo '?')"
echo "  CV=$CV"
echo "  Each cell is ONE run of the strict arm with CRATONVM_ENFORCE_NATIVE_SHADOW"
echo "  armed on that prefix ALONE."
echo
if [ -z "$SKIP_CONTROL" ]; then
    echo "  control (unarmed --jdk-only): $CONTROL_PASSED / $CONTROL_TOTAL${CONTROL_FAILED:+   already red: $CONTROL_FAILED}"
    if [ -n "$CONTROL_FAILED" ]; then
        echo "  NOTE: the control is not clean. Every 'net' column below excludes these,"
        echo "        but a family whose only objection is a vector that was ALREADY red"
        echo "        has not been priced — it has been excused."
    fi
else
    echo "  control: SKIPPED (--skip-control). The 'net' column is the raw failing set."
fi
echo
printf '  %-46s %10s  %s\n' "prefix" "passed" "failed, net of control"
printf '  %-46s %10s  %s\n' "----------------------------------------------" "----------" "----------------------"
printf '%s' "$ROWS" | while IFS='	' read -r p pa to net; do
    [ -n "$p" ] || continue
    n=0; for v in $net; do n=$((n+1)); done
    printf '  %-46s %10s  %s\n' "$p" "$pa / $to" "$n${net:+   $net}"
done
echo
if [ -n "$QUARANTINED" ]; then
    echo "  QUARANTINED — measured flaky, EXCLUDED from every cell above"
    printf '%s' "$QLOG" | awk -F'\t' '
        $3 == "FAIL"   { f[$2]++ }
        $3 != "absent" { n[$2]++ }
        $1 == "control" && $3 == "FAIL" { ctl[$2] = "FAIL" }
        $1 == "control" && $3 == "pass" { ctl[$2] = "pass" }
        END {
            for (v in n)
                printf "    %-28s failed %d of %d arms (control: %s)\n",
                       v, f[v] + 0, n[v], (v in ctl ? ctl[v] : "?")
        }' | sort
    echo "    Rates and records: regression-suite/known-flaky.txt. A cell above is"
    echo "    NOT scored on these, because a 25% vector moves a set-keyed cell three"
    echo "    runs in eight by chance and a gate that cries wolf adjudicates nothing."
    # The blindfold check. A vector that fails EVERYWHERE is not flaky any more.
    printf '%s' "$QLOG" | awk -F'\t' '
        $3 == "FAIL"   { f[$2]++ }
        $3 != "absent" { n[$2]++ }
        END { for (v in n) if (f[v] == n[v]) print v }' | while read -r qv; do
        [ -n "$qv" ] || continue
        echo
        echo "  !! '$qv' is quarantined but failed in EVERY arm of this run, control"
        echo "     included. That is not flakiness. The quarantine is now HIDING a"
        echo "     deterministic failure — re-measure its rate, and if it has stopped"
        echo "     being flaky, remove the row and let the cells score it again."
    done
    echo
fi
if [ -n "$COMMON" ]; then
    echo "  COMMON FACTOR — fails under EVERY armed prefix: $COMMON"
    echo "    H0-4 §4 found exactly this and it was ONE defect with four faces, not four"
    echo "    defects: real bytecode iterating a table the VM never populated. Diagnose a"
    echo "    common-factor row ONCE and every cell above re-prices."
    echo
fi
cat <<'CAVEATS'
  READ BEFORE QUOTING ANY NUMBER ABOVE
    * The cells DO NOT SUM. Each prefix was armed alone; the cost of arming
      several together is a different run (--prefixes 'a+b') that nobody has
      taken. No total is printed on purpose.
    * The prefixes are NOT DISJOINT IN EFFECT. LinkedHashMap extends HashMap
      and HashSet is backed by a HashMap, so arming one already enforces
      shadows the other's run touches.
    * A green cell means "these vectors raise no objection", not "this family
      is retirable". The corpus has no AWT vector at all (G79-1).
    * Cells taken BEFORE 2026-08-21 are void, and not in the direction the
      old caveat here claimed. The dial then reached 1 of 14 dispatch doors
      (890 of 947 armed Bridge dispatches never asked it); it reaches all 14
      now. Half-armed measured MORE damage, not less -- HashMap 81/104 then,
      86/104 once every door consulted the dial -- because half-armed is a
      corrupt hybrid, not a partial retirement. Do not treat an old cell as a
      conservative version of a current one.
    * One conservatism survives and this one IS a floor: a yield needs
      concrete bytecode to yield to, so a triple whose shadowed method has no
      Code still runs its native. --jdk-only-report's
      enforcement_dial.declined_no_bytecode counts exactly those.
    * Four of six witnesses are BLIND (H17-2 2): bucket head class, modCount,
      key hashCode() counts, equals() counts. Only the `table` array class
      discriminates, and it is ONE BIT PER MAP, never a count. Spot-check a
      cell with regression-suite/probes/DialWitness.java, not with a witness
      picked at random.
CAVEATS
echo

# --- adjudication ----------------------------------------------------------
if [ ! -f "$BASELINE" ]; then
    echo "NO BASELINE for key '$KEY' ($BASELINE)."
    echo "  The table above is the measurement; nothing scored it. A baseline taken on"
    echo "  another key cannot adjudicate this one, and 'no baseline' must never read as"
    echo "  'nothing moved'. Take one with:"
    echo "    bash scripts/jdk-only-blast-radius.sh --update-baseline --note '<why now>'"
    exit 2
fi
BL_ADJ="$(sed -n 's/^!adjudicating	//p' "$BASELINE" | head -1)"
if [ "$BL_ADJ" != "yes" ]; then
    echo "BASELINE PRESENT BUT NOT ADJUDICATING ($BASELINE, !adjudicating=$BL_ADJ)."
    echo "  It is a recorded reference, not a scoreboard — usually because it was"
    echo "  transcribed from a record rather than produced by this script on this key."
    echo "  Reference rows, for eyeballing against the table above:"
    grep -v '^[#!]' "$BASELINE" | sed 's/^/    /'
    exit 2
fi
BL_CORPUS="$(sed -n 's/^!corpus	//p' "$BASELINE" | head -1)"
moved=0
echo "ADJUDICATION against $BASELINE"

# The quarantine list is an INPUT to every cell, and adjudication cannot see it.
# Changing it moves cells: quarantining a vector reports REPAIRED under every
# prefix it appeared under, un-quarantining reports REGRESSION. Both are honest
# classifications of a real difference and both send a reader hunting for a code
# change that never happened. So classify this difference too.
BL_QUAR="$(sed -n 's/^!quarantined	//p' "$BASELINE" | head -1)"
if [ "$BL_QUAR" != "$QUARANTINED" ]; then
    echo "  !! THE QUARANTINE LIST CHANGED SINCE THIS BASELINE WAS TAKEN."
    echo "     baseline: ${BL_QUAR:-(none)}"
    echo "     now:      ${QUARANTINED:-(none)}"
    echo "     Cells below WILL move as a result, and that movement is the list"
    echo "     changing rather than the VM. Re-take the baseline once you have"
    echo "     satisfied yourself the list is right — regression-suite/known-flaky.txt"
    echo "     requires a measured rate and a record for every row."
    echo
fi
printf '%s' "$ROWS" | {
  while IFS='	' read -r p pa to net; do
    [ -n "$p" ] || continue
    was="$(awk -F'\t' -v k="$p" '$1==k {print $4}' "$BASELINE" | head -1)"
    if ! grep -q "^$(printf '%s' "$p" | sed 's/[].[^$\\*/]/\\&/g')	" "$BASELINE"; then
        echo "  $p: NOT IN BASELINE — this prefix has never been scored on this key."
        continue
    fi
    diffs=""
    for v in $net; do
        in_set "$v" "$was" && continue
        if [ -n "$BL_CORPUS" ] && ! in_set "$v" "$BL_CORPUS"; then
            diffs="$diffs NEW-VECTOR:$v"
        else
            diffs="$diffs REGRESSION:$v"; moved=1
        fi
    done
    for v in $was; do
        in_set "$v" "$net" && continue
        if [ -n "$CONTROL_SCHEDULED" ] && ! in_set "$v" "$CONTROL_SCHEDULED"; then
            diffs="$diffs RETIRED:$v"
        else
            diffs="$diffs REPAIRED:$v"; moved=1
        fi
    done
    if [ -z "$diffs" ]; then printf '  %-46s unchanged\n' "$p"
    else                    printf '  %-46s%s\n' "$p" "$diffs"; fi
  done
  [ "$moved" -eq 0 ]
} || moved=1

if [ -z "$BL_CORPUS" ]; then
    echo "  NOTE: this baseline records no !corpus line, so a newly-failing vector cannot"
    echo "        be told apart from a regression and is reported as REGRESSION. Re-take"
    echo "        the baseline to fix the classification."
fi

if [ "$moved" -ne 0 ]; then
    echo
    echo "A CELL MOVED. That is the signal this instrument exists to give — the absolute"
    echo "values are expected to be red and are not the point. A REGRESSION means a"
    echo "retirement got MORE expensive; a REPAIRED means it got cheaper and the plan that"
    echo "quotes the old number is stale. Adjudicate, then re-take the baseline."
    exit 1
fi
echo "  every scored cell unchanged."
exit 0
