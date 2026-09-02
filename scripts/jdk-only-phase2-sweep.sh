#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
#
# jdk-only-phase2-sweep.sh — arm the §1.4 shadow dial one receiver at a time.
#
# ---------------------------------------------------------------------------
# What this produces, and what it does NOT
# ---------------------------------------------------------------------------
#
# It produces CANDIDATES. It does not produce verdicts, and the difference is
# the whole finding of the 2026-08-30 adjudication
# (`docs/known-issues/jdk-only/phase-2-adjudicated-the-corpus-cannot-decide-a-retirement-20260830.md`):
#
#   * 270 classes armed individually -> 34 load-bearing, 236 not;
#   * arming those 236 TOGETHER fails 54 of 118 corpus vectors and breaks 35 of
#     78 probe families, with 14 123 530 dispatches reaching the dial and ZERO
#     leaks. The retirement was complete and it still failed.
#
# So a green row here is the first of four preconditions, never the licence.
# The other three are in `docs/contributing/jdk-only-lane-operations.md` §7;
# `jdk-only-phase2-battery.sh` is the instrument for the second.
#
# ---------------------------------------------------------------------------
# THREE verdicts, because "zero of nothing is zero"
# ---------------------------------------------------------------------------
#
#   RETIRE-SAFE   the vector set passes armed AND the dial is confirmed fired
#   LOAD-BEARING  something fails armed -- the failing set names the reason
#   UNKNOWN       no reports were produced, so the dial check is VACUOUS
#
# The third exists because the first draft of this driver printed
# "native-won-still-armed=0 (0 = the dial fired)" for a run that produced ZERO
# reports. A check whose input can be empty has to say so rather than pass.
#
# **And a RETIRE-SAFE row is still vacuous if the vector set never DISPATCHED
# anything on that class.** In the original sweep the 14-vector smoke set
# reached only 120 of 270 classes, so 146 of the 236 greens were the green of a
# question never posed. Intersect the verdicts against the `native-won` rows of
# an UNARMED full-corpus report before reading the column; `reached > 0` in
# `--jdk-only-report`'s `enforcement_dial` is the per-run form of the same
# check.
#
# ---------------------------------------------------------------------------
# The scope is a PREFIX, not a class
# ---------------------------------------------------------------------------
#
# `EnforceShadowScope::covers` is `starts_with`, so a row labelled
# `java/io/File` also armed `FileInputStream`/`FileOutputStream`, and
# `java/util/HashMap` also armed `$KeyIterator`/`$EntrySet`/`$Node`. A row is a
# claim about a prefix. It also means a safe prefix cannot exclude a
# load-bearing class beneath it -- check for that before assembling any
# combined armed set.
#
# ---------------------------------------------------------------------------
# Usage
# ---------------------------------------------------------------------------
#
#   CV=/path/to/cratonvm JDK=/path/to/jdk WORKTREE=/path/to/repo \
#   CLASSES=classes.txt OUT=sweep.log [ONLY="RJdkHello RStrings ..."] \
#     scripts/jdk-only-phase2-sweep.sh
#
# CLASSES is one internal class name per line — build it from the `native-won`
# rows of `--jdk-only-report`, which is the set that has a shadow to retire.
#
# Resumable: a class already in OUT is skipped, so an interrupted sweep costs
# one class rather than the run.
set -u

: "${CV:?set CV to the cratonvm binary under test}"
: "${JDK:?set JDK to the java-home the corpus runs against}"
: "${WORKTREE:?set WORKTREE to the repo root holding regression-suite/}"
: "${CLASSES:?set CLASSES to a file of internal class names, one per line}"
: "${OUT:=jdk-only-phase2-sweep.log}"

# The default is a smoke set: `run.sh` compiles only what ONLY names, which is
# what makes per-class affordable. It is ALSO what makes most greens vacuous —
# see above. The full corpus is ~8x the cost per class.
: "${ONLY:=RJdkHello RStrings RCollections RExceptions RReflect RNumbers RSerial RJdkCollections RJdkNio RConcurrent RJdkStrict RJdkReflect RJdkLambdas RJdkViews}"

cd "$WORKTREE" || exit 1
export CV JDK
export CRATONVM_DISABLE_DEFAULT_WATCHDOG=1

REP=$(mktemp -d "${TMPDIR:-/tmp}/p2rep.XXXXXX")
trap 'rm -rf "$REP"' EXIT

# PIN THE BINARY. The first pass of the original sweep lost 78 rows because the
# binary under it was republished mid-run: the rows either side of that point
# are two measurements wearing one log. Re-checked every iteration so the sweep
# STOPS rather than silently mixing.
VMSHA=$(sha256sum "$CV" | cut -c1-16)

touch "$OUT"
printf 'START %s  vectors=%s  vm=%s\n' "$(date -Is)" "$(echo $ONLY | wc -w)" "$VMSHA" >> "$OUT"

while read -r cls; do
  [ -n "$cls" ] || continue
  grep -q "^$cls " "$OUT" && continue

  now=$(sha256sum "$CV" | cut -c1-16)
  if [ "$now" != "$VMSHA" ]; then
    printf 'ABORT %s binary changed %s -> %s; every row after this point would be a different measurement\n' \
      "$(date -Is)" "$VMSHA" "$now" >> "$OUT"
    exit 3
  fi

  rm -rf "$REP"; mkdir -p "$REP"
  summary=$(CRATONVM_ENFORCE_NATIVE_SHADOW="$cls" ONLY="$ONLY" \
            CRATONVM_ARGS=--jdk-only KEEP_JDK_ONLY_REPORTS="$REP" \
            bash regression-suite/run.sh 2>&1 | grep -E "^REGRESSION SUITE:" | head -1)

  if [ "$(ls "$REP"/*.json 2>/dev/null | wc -l)" -eq 0 ]; then
    printf '%-52s UNKNOWN      no reports; %s\n' "$cls" "${summary:-no summary}" >> "$OUT"
    continue
  fi

  # Did the dial fire for THIS scope? Zero surviving `native-won` rows is the
  # proof, and it is only meaningful because there are reports to look in.
  still=$(python3 - "$REP" "$cls" <<'PY'
import glob, json, sys
rep, cls = sys.argv[1], sys.argv[2]
n = 0
for p in glob.glob(rep + "/*.json"):
    try:
        d = json.load(open(p, encoding="utf-8"))
    except Exception:
        continue
    for v in d.get("violations") or []:
        if (v.get("kind") == "native-shadows-bytecode"
                and v.get("outcome") == "native-won"
                and (v.get("class") or "").startswith(cls)):
            n += 1
print(n)
PY
)
  case "$summary" in
    *", 0 failed"*) verdict=RETIRE-SAFE ;;
    *)              verdict=LOAD-BEARING ;;
  esac
  # Armed, but the scope's natives still won: the dial did not take effect for
  # it, so neither verdict above is earned.
  [ "$still" != "0" ] && verdict=DIAL-INERT

  printf '%-52s %-12s still_native=%-4s %s\n' "$cls" "$verdict" "$still" "$summary" >> "$OUT"
done < "$CLASSES"

printf 'DONE %s\n' "$(date -Is)" >> "$OUT"
