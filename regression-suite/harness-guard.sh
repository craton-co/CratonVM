#!/usr/bin/env bash
# The regression suite's INSTRUMENT CHECK.
#
# WHY THIS FILE EXISTS
# --------------------
# run.sh reduces every vector's output to its PASS/CK lines before diffing the
# two VMs (see extract() below). That filter is right — CratonVM interleaves
# timestamped WARN/tracing noise with the vector's output and a raw diff would
# be red on every class for reasons that have nothing to do with the VM — but
# for a long time NOTHING CHECKED THAT ANYTHING MEANINGFUL SURVIVED IT.
#
# Three scheduled vectors printed their entire evidence on other prefixes.
# extract() reduced each of them to the constant `PASS <Class>`, and a constant
# string always matches itself. Measured side by side in
# W7-51-vacuous-sweep-round-2.md: RDataInputFastPull with a one-line defect
# injected — a typed read dropping the high byte of readShort(), exactly the
# partial fast pull the vector exists to catch — exited rc=0 with output
# BYTE-IDENTICAL to a clean run. The vector was scheduled, it ran, and it could
# not fail.
#
# Repairing those three vectors fixes three vectors. It does not stop the
# fourth. This file is the part that does: it makes the harness state, per
# vector and on every run, that the comparison it is about to report on is a
# comparison it can actually lose.
#
# THE FOUR GUARDS
# ---------------
#   G1 DISCARDED EVIDENCE   the oracle printed a line extract() deletes.
#   G2 CONSTANT EXTRACT     what survives extract() carries no observable at
#                           all, so the cross-VM diff compares a constant
#                           against itself.
#   G3 NO CHECK COUNT       the vector publishes no count of the assertions it
#                           executed, so a run that silently asserted FEWER
#                           things than the oracle is indistinguishable from a
#                           run that asserted all of them. Ratcheted, not
#                           fatal-on-sight — see harness-uncounted.txt.
#   G4 SICK ORACLE          the HotSpot run that supplies ground truth did not
#                           itself succeed, so the "expected" side of the diff
#                           is a truncated artefact of the oracle's failure.
#
# A guard that has never been shown to fire is the same species of defect as
# the one it is guarding against. All four were mutation-checked; the evidence
# is in docs/known-issues/jdk-only/W7-60-harness-extract-blindness.md and the
# mutants are reproducible with harness-selfcheck.sh, which runs the guards
# against HotSpot alone and therefore needs no CratonVM build.
#
# Sourced by run.sh and by harness-selfcheck.sh. Defines exactly one copy of
# extract(), so the filter the suite diffs through and the filter the guards
# reason about cannot drift apart.

# Extract only the deterministic test lines (PASS/CK), stripping CratonVM's
# timestamped WARN/tracing noise and ANSI colour, so the cross-VM diff is clean.
extract() { sed 's/\x1b\[[0-9;]*m//g' | grep -aE '^(PASS|CK) ' ; }

# Lines a JDK writes to its own stderr that are NOT vector evidence, and so are
# legitimately dropped. Deliberately a MEASURED list, not a defensive one: the
# only shape that occurs across the 70 scheduled vectors on Temurin 25.0.3+9 is
# the restricted-method WARNING block from System.loadLibrary (RJdkJni,
# RJdkFailure, four lines each). Widening this pattern is how G1 would be
# talked out of firing, so widen it only against a measurement.
HARNESS_NOISE_RE='^(WARNING: |Picked up (JAVA_TOOL_OPTIONS|_JAVA_OPTIONS)|OpenJDK [0-9A-Za-z_-]+ (Server )?VM warning:)'

# Does this extracted block publish how many assertions ran? Two accepted
# spellings, both of which survive extract():
#     PASS RFoo (42 checks)
#     CK RFoo checks=42
# Zero is not a count: a vector that reports `checks=0` executed no assertion.
harness_check_count() {
  # $1 = class, stdin = extracted output. Echoes the count, or nothing.
  awk -v cls="$1" '
    $0 ~ "^CK " cls " checks=" { sub(/^.*checks=/, ""); print; found=1; exit }
    $0 ~ "^PASS " cls "( |$)" {
      if (match($0, /\(([0-9]+) checks?\)/)) {
        s = substr($0, RSTART + 1, RLENGTH - 2); sub(/ checks?$/, "", s); print s; found=1; exit
      }
    }
  '
}

# Read harness-uncounted.txt into $HARNESS_UNCOUNTED (space-delimited, with
# leading and trailing spaces so `case` membership tests are exact).
harness_load_uncounted() {
  HARNESS_UNCOUNTED=" "
  [ -f "$1" ] || return 0
  while IFS= read -r line; do
    line=${line%%#*}
    for w in $line; do HARNESS_UNCOUNTED="$HARNESS_UNCOUNTED$w "; done
  done < "$1"
}

# ---------------------------------------------------------------------------
# harness_guard_extract <class> <extracted-file>
#
# G2 and G3. Needs only what survived the filter, so it runs on every invocation
# — including on a host with no HotSpot, where the cross-VM diff is skipped for
# every class and these are the ONLY thing standing between "the vector printed
# its banner" and "the vector measured something".
#
# Appends one line per violation to $HARNESS_GUARD_MSGS and returns 1 if any.
# ---------------------------------------------------------------------------
harness_guard_extract() {
  gc_class="$1"; gc_key="$2"; gc_bad=0
  # `grep -c` prints its count AND exits 1 when the count is zero, so a
  # `|| echo 0` fallback would append a SECOND number and every arithmetic test
  # below would break on "0\n0". Take the count and default the empty case.
  gc_lines=$(grep -ac . "$gc_key" 2>/dev/null); gc_lines=${gc_lines:-0}
  # Any `CK ` line counts as an observable. Deliberately NOT anchored on the
  # class name: several vectors label their CK lines by the SUBJECT rather than
  # the class (`CK tm-range 1234`, `CK pbq-sorted 5678`), and those carry a real
  # measurement. Only the check-count parse below is class-anchored, because
  # that one has to read a number out of a specific line.
  gc_ck=$(grep -ac '^CK ' "$gc_key" 2>/dev/null); gc_ck=${gc_ck:-0}
  gc_count=$(harness_check_count "$gc_class" < "$gc_key")

  # G2. An observable is a CK line or a published check count. Without one of
  # the two, everything that reaches the diff is fixed text that the vector
  # would print whatever the VM did with it.
  if [ "$gc_ck" -eq 0 ] && [ -z "$gc_count" ]; then
    if [ "$gc_lines" -eq 0 ]; then
      HARNESS_GUARD_MSGS="$HARNESS_GUARD_MSGS
  HARNESS ERROR [G2] $gc_class: nothing survives extract() — the cross-VM diff compares two empty strings"
    else
      HARNESS_GUARD_MSGS="$HARNESS_GUARD_MSGS
  HARNESS ERROR [G2] $gc_class: extract() leaves a CONSTANT ($gc_lines line(s), no CK line, no check count).
    A constant always matches itself, so this vector cannot fail the diff. Print
    its observables on 'CK $gc_class <key>=<value>' lines, or publish a check count."
    fi
    gc_bad=1
  fi

  # G3. Ratcheted against harness-uncounted.txt in BOTH directions: a vector
  # that stops publishing a count is a regression, and a vector that starts
  # publishing one is not repaired until its row is deleted. A baseline that
  # only records "known bad" decays into a list nobody re-checks.
  case "$HARNESS_UNCOUNTED" in
    *" $gc_class "*) gc_listed=1 ;;
    *) gc_listed=0 ;;
  esac
  if [ -z "$gc_count" ] || [ "$gc_count" -eq 0 ] 2>/dev/null; then
    if [ "$gc_listed" -eq 0 ]; then
      HARNESS_GUARD_MSGS="$HARNESS_GUARD_MSGS
  HARNESS ERROR [G3] $gc_class: publishes no check count, and is not in regression-suite/harness-uncounted.txt.
    Without a count, a run that silently asserted FEWER things than the oracle
    diffs identically. Emit 'PASS $gc_class (N checks)' or 'CK $gc_class checks=N'."
      gc_bad=1
    fi
  elif [ "$gc_listed" -eq 1 ]; then
    HARNESS_GUARD_MSGS="$HARNESS_GUARD_MSGS
  HARNESS ERROR [G3] $gc_class: publishes a check count ($gc_count) but is still listed in
    regression-suite/harness-uncounted.txt. Delete its row — the ratchet only holds
    if clearing an entry is what makes the run green again."
    gc_bad=1
  fi
  return $gc_bad
}

# ---------------------------------------------------------------------------
# harness_guard_oracle <class> <oracle-raw-file> <oracle-rc>
#
# G1 and G4. Both need the ORACLE's raw output specifically, and the reason is
# the whole trick: HotSpot prints exactly what the vector prints and nothing
# else, so the set of lines extract() drops from IT is precisely the evidence
# the harness is blind to. The same subtraction against CratonVM's output would
# be swamped by the VM's own tracing and could never be made fatal.
# ---------------------------------------------------------------------------
harness_guard_oracle() {
  go_class="$1"; go_raw="$2"; go_rc="$3"; go_bad=0

  # G4 first: everything G1 says about a sick oracle's output is uninteresting.
  if [ "$go_rc" -ne 0 ]; then
    go_why="rc=$go_rc"
    [ "$go_rc" -eq 124 ] && go_why="rc=124 (TIMED OUT — its output is a truncated prefix)"
    HARNESS_GUARD_MSGS="$HARNESS_GUARD_MSGS
  HARNESS ERROR [G4] $go_class: the HotSpot oracle run FAILED ($go_why), so the
    'expected' side of the cross-VM diff is an artefact of the oracle's failure,
    not ground truth. First dropped line: $(grep -avE "$HARNESS_NOISE_RE" "$go_raw" | grep -a . | head -1 | head -c 100)"
    return 1
  fi
  if ! grep -qaE "^PASS $go_class([^A-Za-z0-9_]|\$)" "$go_raw"; then
    HARNESS_GUARD_MSGS="$HARNESS_GUARD_MSGS
  HARNESS ERROR [G4] $go_class: the HotSpot oracle exited 0 but printed no 'PASS $go_class' line."
    return 1
  fi

  # G1. This is the defect the file is named for, stated as a predicate.
  go_dropped=$(grep -a . "$go_raw" | grep -avE '^(PASS|CK) ' | grep -avE "$HARNESS_NOISE_RE")
  if [ -n "$go_dropped" ]; then
    go_n=$(printf '%s\n' "$go_dropped" | grep -c .)
    HARNESS_GUARD_MSGS="$HARNESS_GUARD_MSGS
  HARNESS ERROR [G1] $go_class: the oracle printed $go_n line(s) that extract() DELETES before the diff.
    Evidence on a non-PASS/CK prefix is evidence the suite cannot see. Move it to
    'CK $go_class <key>=<value>'. Dropped:
$(printf '%s\n' "$go_dropped" | head -6 | sed 's/^/      | /')"
    go_bad=1
  fi
  return $go_bad
}
