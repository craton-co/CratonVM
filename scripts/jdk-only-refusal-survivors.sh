#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
#
# jdk-only-refusal-survivors.sh — the gate for refusals that did NOT retire.
#
# ---------------------------------------------------------------------------
# The species
# ---------------------------------------------------------------------------
#
# `NativeMethodRegistry::register` is last-write-wins, and its `JdkOnly` arm
# returns WITHOUT inserting. So refusing a `SyntheticStub` retires the method to
# real JDK bytecode only when nothing already owns the triple. When an EARLIER
# registration does:
#
#     the earlier native survives as the winner instead.
#
# The retirement silently does not happen, and the two modes run different code
# for that triple. Worse for the metric: when the survivor's kind is
# `Intrinsic`, the §1.4 shadow census never counts it either — every recorder
# skips `NativeKind::Intrinsic` — so the row was an invisible non-retirement AND
# invisible to the number that is supposed to track retirements.
#
# ---------------------------------------------------------------------------
# Why a script and not a unit test
# ---------------------------------------------------------------------------
#
# `native-api/tests/jdk_only_registry.rs` pins the MECHANISM on a two-line
# registry, and it is the cheaper test to keep green. It cannot pin the
# SHIPPING registry, and neither can the two existing census tests:
#
#   * `native-builtins/tests/registrar_drift.rs` compares a synthetic-only pass
#     against a shipping one. This species is two SHIPPING passes, so it is
#     structurally outside that comparison;
#   * `native-builtins/tests/duplicate_registration_gate.rs` records it as its
#     own blind spot 3 — "a DROPPED registration leaves no row at all", because
#     every drop arm in `register()` returns before pushing to `registrations`.
#     A shadowed LOSER has a census row; a refused one does not.
#
# So this gate boots the real VM under `--jdk-only`, reads the refusals out of
# the `--jdk-only-report`, and compares the ones that left a survivor against a
# frozen baseline. A new row is a new instance of the species and fails.
#
# ---------------------------------------------------------------------------
# What the baseline records, and what it deliberately does not
# ---------------------------------------------------------------------------
#
# Four TSV columns: class, method, descriptor, survivor KIND. The survivor's
# `file:line` is in the report and NOT in the baseline on purpose — an unrelated
# edit above the registration would move the line and fail a gate that has
# nothing to say about it. The kind is the load-bearing half: it is what decides
# whether any other census can see the row at all.
#
# Usage:
#   CV=/path/to/cratonvm JDK=/path/to/jdk bash scripts/jdk-only-refusal-survivors.sh
#   UPDATE_BASELINE=1 ... # rewrite the baseline after a reviewed change
set -uo pipefail

CV=${CV:-target/release/cratonvm}
JDK=${JDK:-${JAVA_HOME:-}}
BASELINE=${BASELINE:-scripts/baselines/jdk-only-refusal-survivors.tsv}

if [ ! -x "$CV" ]; then
  echo "ERROR: CV='$CV' is not an executable CratonVM binary" >&2
  exit 2
fi
if [ -z "$JDK" ] || [ ! -x "$JDK/bin/javac" ]; then
  echo "ERROR: JDK='$JDK' does not look like a JDK home (no bin/javac)" >&2
  exit 2
fi

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

cat > "$WORK/RefusalSurvivorProbe.java" <<'EOF'
// The registry is populated at VM init, so any main class at all measures the
// whole shipping registrar sequence. This one is deliberately trivial: a probe
// that loaded frameworks would make the answer depend on what it loaded.
public class RefusalSurvivorProbe {
    public static void main(String[] a) {
        System.out.println("RefusalSurvivorProbe");
    }
}
EOF
"$JDK/bin/javac" -d "$WORK" "$WORK/RefusalSurvivorProbe.java" || exit 2

"$CV" --java-home "$JDK" --jdk-only --jdk-only-report "$WORK/report.json" \
      -cp "$WORK" RefusalSurvivorProbe >"$WORK/out.txt" 2>"$WORK/err.txt"
rc=$?
if [ ! -s "$WORK/report.json" ]; then
  echo "ERROR: no --jdk-only-report was written (VM rc=$rc); last stderr:" >&2
  tail -20 "$WORK/err.txt" >&2
  exit 2
fi

python3 - "$WORK/report.json" > "$WORK/measured.tsv" <<'PY'
import json, sys

report = json.load(open(sys.argv[1]))

def rows(node):
    """Every violation object anywhere in the report, whatever it is nested in.

    The report's shape is owned by `vm_init.rs`, not by this script; walking it
    beats hard-coding a path that a schema bump would silently empty. An empty
    result is treated as a FAILURE below rather than as a clean gate.
    """
    if isinstance(node, dict):
        if node.get("kind") == "synthetic-native-registered":
            yield node
        for v in node.values():
            yield from rows(v)
    elif isinstance(node, list):
        for v in node:
            yield from rows(v)

seen = set()
total = 0
for r in rows(report):
    total += 1
    survivor = r.get("survivor")
    if not survivor:
        continue
    kind = survivor.split("@", 1)[0]
    seen.add((r.get("class", "?"), r.get("method", "?"),
              r.get("descriptor", "?"), kind))
print("# refusals seen: %d" % total)
for row in sorted(seen):
    print("\t".join(row))
PY

if [ "$(grep -c . "$WORK/measured.tsv")" = "0" ]; then
  echo "ERROR: the report parsed to nothing — schema change?" >&2
  exit 2
fi

# A run with ZERO refusals is not a green gate, it is a broken measurement:
# strict mode refuses ~1,500 registrations on this tree, and a zero means the
# policy never engaged (wrong flag, wrong binary, report written before init).
refusals=$(sed -n 's/^# refusals seen: //p' "$WORK/measured.tsv")
if [ "${refusals:-0}" -eq 0 ]; then
  echo "ERROR: the report contains no refusals at all — --jdk-only did not engage" >&2
  exit 2
fi

grep -v '^#' "$WORK/measured.tsv" | sort > "$WORK/measured.sorted"

if [ "${UPDATE_BASELINE:-0}" = "1" ]; then
  {
    echo "# Refusals under --jdk-only that left an EARLIER native owning the slot."
    echo "# class<TAB>method<TAB>descriptor<TAB>survivor-kind"
    echo "# Regenerate: UPDATE_BASELINE=1 bash scripts/jdk-only-refusal-survivors.sh"
    cat "$WORK/measured.sorted"
  } > "$BASELINE"
  echo "baseline rewritten: $BASELINE ($(grep -vc '^#' "$BASELINE") rows)"
  exit 0
fi

if [ ! -f "$BASELINE" ]; then
  echo "ERROR: baseline '$BASELINE' is missing" >&2
  exit 2
fi
grep -v '^#' "$BASELINE" | sort > "$WORK/baseline.sorted"

if diff -u "$WORK/baseline.sorted" "$WORK/measured.sorted" > "$WORK/diff.txt"; then
  echo "jdk-only refusal survivors: $(grep -c . "$WORK/measured.sorted") rows, matches baseline (refusals seen: $refusals)"
  exit 0
fi

echo "FAIL: the set of refusals that did NOT retire has changed." >&2
echo "      A '+' row is a NEW instance of the species: a --jdk-only refusal" >&2
echo "      landed on a triple an earlier registration already owned, so the" >&2
echo "      method did not fall through to bytecode — the older native still" >&2
echo "      serves, and the two modes now run different code for it." >&2
echo "      A '-' row is one that closed; re-run with UPDATE_BASELINE=1." >&2
cat "$WORK/diff.txt" >&2
exit 1
