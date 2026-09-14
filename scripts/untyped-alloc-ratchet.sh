#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# untyped-alloc-ratchet.sh — watch the fabricated-carrier surface.  VERSION 7.
#
# WHAT IT COUNTS
# --------------
# `ClassId::new(0)` is the untyped sentinel. Passed to an OBJECT allocator the
# VM substitutes `cratonvm/synthetic/AnonymousObject$N` (`vm/src/vm/vm_exec.rs`);
# passed to an ARRAY allocator it produces a bare `Object[]` where the real class
# declares a typed array. The caller reached that line because it *resolved a
# class and FAILED*, then handed the result out as the thing it named.
#
# `H0-6` §7: `AnonymousObject$4` IS the `HashMap.Node`. `H0-4` §7: the table is
# fully populated and `size()`/`get()` are correct — ITERATION breaks, because
# the nodes are the wrong class. `H23-1`: the array holding them was wrong too.
# `native-builtins/tests/stub_ratchet.rs` cannot see any of this — it counts
# REGISTRATIONS, and a substitution is not a registration.
#
# THE COUNTING RULE, stated because five earlier versions each got it wrong
# --------------------------------------------------------------------------
# Match is by an explicit ALLOWLIST of allocator functions, with a per-function
# rule for the second argument:
#
#   alloc_object, alloc_object_of   COUNT unless the width is literal `0`.
#                                   The VM substitutes only when `num_fields > 0`
#                                   (`vm_exec.rs`), so a literal 0 fabricates
#                                   nothing.
#   new_ref_array, try_new_ref_array  COUNT regardless of length — including a
#                                   literal 0 and, crucially, a VARIABLE. An
#                                   array length is naturally dynamic, so a
#                                   literal-only pattern is blind to most of the
#                                   population by construction: 29 of 147.
#
# NOT allocators, deliberately excluded: `class_is` (a comparison) and
# `define_class` (the sentinel means "no parent", not "unknown class"). A
# blanket "any function name" pattern counts both and over-reports.
#
# UNKNOWN spellings are DISCOVERED but not counted: the breakdown finds every
# function name that takes the sentinel, so a new allocator appears as a named
# line and trips the new-spelling check, asking a human to classify it. That is
# the fail-safe direction — v1 and v2 were wrong precisely because a spelling
# they had never seen was silently absent rather than loudly unclassified.
#
# WHAT V7 CHANGED, and both changes MOVE THE NUMBERS
# --------------------------------------------------
# (a) `#[cfg(test)] mod` BLOCKS ARE NOW EXCLUDED, not just test FILES.
#     MEASURED 2026-08-21 at 22cb4338d: **197 of the 359 sites v6 counted —
#     54.9% — are inside a `#[cfg(test)] mod` block of a PRODUCTION file**, so
#     more than half of the headline number was test fixtures allocating test
#     objects. v6's `EXCL` is a git PATHSPEC and cannot see inside a file.
#     The worked example is the one v6's own header calls out: `t27_tls.rs` was
#     deliberately kept as production source, and 35 of its 48 sites are inside
#     `mod tests` (lines 8177-11577). Keeping the file and dropping its test
#     module is what v6 meant to do and could not express.
#
#     **The drop from 359 to 162 is NOT an improvement — it is a correction.**
#     Nothing was fixed. `test_sites` is printed on every run so the excluded
#     population never goes dark.
#
#     Falsified on the way in: a first attempt tracked `{`/`}` depth and OVERRAN,
#     because braces inside Rust string literals are not braces. A top-level
#     `mod` ends at the next COLUMN-0 `}`, which no string literal can fake.
#
#     CORRECTED 2026-08-22: the `mod` pattern was `^(pub )?mod NAME {`, which
#     does not match `pub(crate) mod new13_tests {` — a real shape, at
#     phases_late/ssl_security.rs:8671. FOUR more sites were counted as
#     production because of it (objects 33 -> 29, excluded 197 -> 201). Found by
#     reading the new per-function reach map and noticing a TEST NAME in it.
#
# (c) The per-function REACH MAP, added 2026-08-22 for a measured reason.
#     `arrays` grew by one and `reach` did not move at all, because WORKER 2's
#     new (deliberate) `degrade_bucket_table_to_untyped` site (+1) exactly
#     cancelled a REMOVED caller of `alloc_ref_array` (157 -> 156). ONE
#     AGGREGATE NUMBER HID A REGRESSION AND AN IMPROVEMENT AT THE SAME TIME.
#     The baseline now carries one `fn=contribution` line per enclosing
#     function, and every run prints the entries that moved — even when the
#     total did not.
#
# (b) `reach` — ONE LEVEL OF CALL GRAPH, which is what v6's LIMIT 7 asked for.
#     v6 counted direct spellings only, so routing a caller of a helper away
#     from an untyped allocation removed a real fabrication and moved the number
#     by ZERO. MEASURED 2026-08-21: `alloc_ref_array` is ONE counted site with
#     **157 in-tree callers**; typing the no-arg `HashMap()` table left
#     `arrays: 151` unchanged.
#
#     reach = SUM over counted sites of max(1, callers(enclosing fn))
#           = 629 against 162 direct sites at 22cb4338d.
#
#     A site in a function nothing calls counts once. A site in a helper counts
#     once per caller. Re-routing a caller now moves a number.
#
# THE LIMITS, PRINTED WITH THE NUMBERS AND NOT ONLY HERE
# ------------------------------------------------------
# L1  `reach` is ONE level. A caller of a caller of `alloc_ref_array` is still
#     invisible; both columns remain FLOORS.
# L2  Attribution is by FUNCTION NAME. A name defined more than once is
#     attributed to whichever definition the grep counts (3 such names today);
#     the run prints `ambiguous=` so the figure is never quoted as exact.
# L3  Grep over source text, not a runtime census. A site behind `cfg` or
#     generated by a macro is invisible. `CRATONVM_DBG_ANONALLOC=1` is the
#     runtime instrument and `H0-6` §8 measured it attributing 4.4% of events,
#     so neither is authoritative alone.
# L4  It ratchets DRIFT. Do not quote it as the size of the problem.
#
# THE HISTORY IS THE ARGUMENT FOR THE GUARDS
# ------------------------------------------
#   v1  bare `ClassId::new(0)` only -> 84 of 203 (41%), printed a clean "ok".
#       Caught by `H16` deleting two sites and watching the number not move.
#   v2  widened the pattern, left `git grep` in BASIC regex where `(` is a
#       literal -> the grouping matched nothing; 89 instead of 203.
#   v3  an edit dropped `EXCL`; printed "IMPROVED by 84 … ok" with **rc=0 while
#       matching NOTHING**.
#   v4  folded `new_ref_array`'s LENGTH into WIDTHS, where it is a field count
#       -> 16/32/64 appeared as three carrier families that do not exist.
#   v5  required a positive literal length -> saw 29 of 147 array sites (19%).
#       `H23` ran the falsifier BY ACCIDENT: its fix removed the sentinel behind
#       every `HashMap.table` and the gate still reported 29 -> 29, green.
#   v6  counted direct spellings only (LIMIT 7) and counted `#[cfg(test)] mod`
#       blocks of production files as production sites — 54.9% of its number.
#   v7a the first `mod` pattern missed `pub(crate) mod`, and a single `reach`
#       total hid an offsetting pair. Both found by this gate's own output.
#
# Every one printed a confident number. **A gate that measures a FRACTION reads
# as good news.** Hence: the zero-guard, the per-function breakdown, the unit
# separation, the printed limits, and a `--selftest` that makes the gate FAIL
# on every one of its failure paths rather than only checking it can match.
#
# Usage:  scripts/untyped-alloc-ratchet.sh [--update|--selftest]
# Env:    RATCHET_BASELINE  alternate baseline path (the selftest uses this)
#         RATCHET_CRATES    alternate search roots (the selftest uses this)
# Exit:   0 ok · 1 ratchet tripped · 2 no baseline · 3 gate is broken
# ---------------------------------------------------------------------------
set -u

cd "$(dirname "$0")/.." || exit 3
BASELINE="${RATCHET_BASELINE:-scripts/baselines/untyped-alloc-sites.txt}"

# `vm/` and `gc/` are excluded on purpose: their sentinel uses are the allocator
# and the collector themselves — where it is defined and consumed, not produced.
CRATES="${RATCHET_CRATES:-native-builtins/src native-collections/src native-io/src
        native-api/src native-builtins-crypto/src native-builtins-security/src
        native-awt/src}"

# Test-support PATHS excluded by pathspec, never by grepping the matched line for
# "test" — `-h` drops the path, turning a path filter into a CONTENT filter that
# keeps `test_mock.rs` and drops any real site whose line says "latest".
# `t27_tls.rs` is deliberately NOT excluded here: production source, test-shaped
# name. Its `mod tests` is dropped by the BLOCK filter in the awk pass instead.
EXCL=':!*/tests/*  :!*test_*.rs  :!*_test.rs  :!*/test_utils.rs  :!*/test_mock.rs'

SENT='\(([A-Za-z0-9_]+::)*ClassId::new\(0\), *'
ANY_PAT="[A-Za-z0-9_]+${SENT}[^)]*\)"             # file selection + discovery

# ---------------------------------------------------------------------------
# PASS 1 — one awk over every file that mentions the sentinel at all.
#
# awk rather than more `git grep` because three of the four things we need are
# POSITIONAL: whether the line is inside a `#[cfg(test)] mod` block, which `fn`
# encloses it, and how many matches the line carries. `git grep -h` throws away
# the file and the line number, which is exactly the information that decides
# all three.
#
# Emits, one record per line:
#   S <kind> <enclosing-fn>   a COUNTED production site (kind = obj|arr)
#   T                         a site suppressed by the cfg(test) block filter
#   W <width>                 a literal carrier width from an object allocator
#   F <fn>                    one discovered spelling occurrence (any function)
# ---------------------------------------------------------------------------
scan() {
  # shellcheck disable=SC2086
  local files
  files=$(git grep -lE "$ANY_PAT" -- $CRATES $EXCL 2>/dev/null)
  [ -z "$files" ] && return 0
  # shellcheck disable=SC2086
  echo "$files" | tr '\n' '\0' | xargs -0 awk '
    function discover(s,   m) {
      while (match(s, /[A-Za-z0-9_]+\(([A-Za-z0-9_]+::)*ClassId::new\(0\), *[^)]*\)/)) {
        m = substr(s, RSTART, RLENGTH); sub(/\(.*/, "", m)
        print "F " m
        s = substr(s, RSTART + RLENGTH)
      }
    }
    function widths(s,   m) {
      while (match(s, /alloc_object(_of)?\(([A-Za-z0-9_]+::)*ClassId::new\(0\), *[1-9][0-9]*\)/)) {
        m = substr(s, RSTART, RLENGTH)
        sub(/.*ClassId::new\(0\), */, "", m); sub(/\).*/, "", m)
        print "W " m
        s = substr(s, RSTART + RLENGTH)
      }
    }
    FNR == 1 { intest = 0; pend = 0; fn = "-" }
    {
      # Half a million lines pass through here, so every regex is behind a
      # literal `index()` guard. Without them this pass took 11.6 s and the
      # selftest, which runs the gate nine times, could not finish.
      c1 = substr($0, 1, 1)

      # A COLUMN-0 `#[cfg(test)]` (optionally followed by more attributes) that
      # introduces a COLUMN-0 `mod X {` opens a test block; it closes at the
      # next COLUMN-0 `}`. Depth counting was tried and OVERRAN — braces inside
      # Rust string literals are not braces, and `mod tests` in t27_tls.rs ran
      # to EOF instead of to line 11577.
      if (pend) {
        if (c1 == "#") next
        # `pub(crate) mod new13_tests {` is a REAL shape in this tree
        # (phases_late/ssl_security.rs:8671) and `^(pub )?mod` did not match it,
        # so that whole test module counted as production. Accept any
        # visibility, and tolerate trailing space before the brace.
        if ($0 ~ /^(pub([ 	]*\([^)]*\))?[ 	]+)?mod[ 	]+[A-Za-z0-9_]+[ 	]*\{[ 	]*$/) { intest = 1; pend = 0; next }
        pend = 0
      }
      if (c1 == "#") { if ($0 == "#[cfg(test)]") { pend = 1; next } }
      else if (intest && c1 == "}") { if ($0 ~ /^\}[ \t]*$/) { intest = 0; next } }

      if (index($0, "fn ") || index($0, "fn\t")) {
        if (match($0, /^[ \t]*(pub([ \t]*\([^)]*\))?[ \t]+)?(default[ \t]+)?(const[ \t]+)?(async[ \t]+)?(unsafe[ \t]+)?(extern[ \t]+"[^"]*"[ \t]+)?fn[ \t]+[A-Za-z0-9_]+/)) {
          fn = substr($0, RSTART, RLENGTH); sub(/.*fn[ \t]+/, "", fn)
        }
      }
      if (index($0, "ClassId::new(0)") == 0) next

      # DISCOVERY is deliberately wider than COUNTING, in BOTH directions.
      #  * every function NAME that takes the sentinel is reported, including
      #    the two excluded from the counts on purpose (`class_is`,
      #    `define_class`), so a new allocator spelling always asks for a human;
      #  * and it runs inside `#[cfg(test)]` blocks too, which the COUNTS do
      #    not. A new spelling or a new carrier width is a new SHAPE whether it
      #    appears in a fixture or in a native, and this is the fail-safe
      #    direction: v1 and v2 were wrong because a spelling they had never
      #    seen was silently absent. It also keeps `byfn` and `widths`
      #    comparable to every baseline v1..v6 wrote.
      if ($0 !~ /[A-Za-z0-9_]+\(([A-Za-z0-9_]+::)*ClassId::new\(0\), *[^)]*\)/) next
      discover($0)
      widths($0)
      isobj = ($0 ~ /alloc_object(_of)?\(([A-Za-z0-9_]+::)*ClassId::new\(0\), *[^)]*\)/) &&
              !($0 ~ /alloc_object(_of)?\(([A-Za-z0-9_]+::)*ClassId::new\(0\), *0\)/)
      isarr = ($0 ~ /(try_)?new_ref_array\(([A-Za-z0-9_]+::)*ClassId::new\(0\), *[^)]*\)/)
      if (intest) { if (isobj || isarr) print "T"; next }
      if (isobj) print "S obj " fn
      if (isarr) print "S arr " fn
    }
  '
}

RAW=$(scan)
OBJ=$(printf '%s\n' "$RAW" | grep -c '^S obj ')
ARR=$(printf '%s\n' "$RAW" | grep -c '^S arr ')
TESTS=$(printf '%s\n' "$RAW" | grep -c '^T$')
SITES=$((OBJ + ARR))

# WIDTHS from the OBJECT allocators only, literal widths only. For an array the
# second argument is a LENGTH, not a field count — v4 mixed the units and
# invented three carrier families.
WIDTHS=$(printf '%s\n' "$RAW" | sed -n 's/^W //p' | sort -n -u | tr '\n' ' ' | sed 's/ $//')
BYFN=$(printf '%s\n' "$RAW" | sed -n 's/^F //p' | sort | uniq -c | sort -rn \
         | awk '{printf "%s=%s ", $2, $1}' | sed 's/ $//')

if [ "$SITES" -eq 0 ]; then
  echo "UNTYPED-ALLOC RATCHET: matched ZERO production sites."
  echo "  That is a broken gate, not a clean tree — this sentinel is used"
  echo "  throughout the native crates. Check the patterns, EXCL and CRATES,"
  echo "  and the cfg(test) block filter, which can swallow a whole file if a"
  echo "  column-0 '}' goes missing."
  exit 3
fi

# ---------------------------------------------------------------------------
# PASS 2 — reach. Two greps, not one per function: 115 enclosing names would be
# 230 `git grep` invocations over a tree this size.
# ---------------------------------------------------------------------------
ENCL=$(printf '%s\n' "$RAW" | sed -n 's/^S [a-z]* //p' | grep -v '^-$' | sort | uniq -c)
DIRECT=$(printf '%s\n' "$RAW" | sed -n 's/^S [a-z]* //p' | grep -c '^-$')
NAMES=$(printf '%s\n' "$ENCL" | awk '{print $2}' | sort -u | paste -sd'|' -)

REACH=$DIRECT
AMBIG=0
if [ -n "$NAMES" ]; then
  # shellcheck disable=SC2086
  CALLS=$(git grep -hoE "\b($NAMES)[[:space:]]*\(" -- $CRATES $EXCL 2>/dev/null \
            | sed -E 's/[[:space:]]*\($//' | sort | uniq -c)
  # shellcheck disable=SC2086
  DEFS=$(git grep -hoE "fn[[:space:]]+($NAMES)[[:space:]]*\(" -- $CRATES $EXCL 2>/dev/null \
            | sed -E 's/^fn[[:space:]]+//; s/[[:space:]]*\($//' | sort | uniq -c)
  # Emits the TOTAL on line 1 and one sorted `fn=contribution` per line after.
  # The per-function map is what makes an OFFSET visible: on 2026-08-22 `arrays`
  # grew by one while `reach` did not move at all, because a new deliberate site
  # (+1) exactly cancelled a REMOVED caller of `alloc_ref_array` (157 -> 156).
  # One aggregate number hid a real regression and a real improvement at once,
  # which is the failure mode this whole file is about.
  REACHMAP=$(printf 'ENCL\n%s\nCALLS\n%s\nDEFS\n%s\n' "$ENCL" "$CALLS" "$DEFS" | awk -v base="$DIRECT" '
    /^ENCL$/  { s = "e"; next }
    /^CALLS$/ { s = "c"; next }
    /^DEFS$/  { s = "d"; next }
    NF == 0   { next }
    s == "e" { encl[$2] = $1; next }
    s == "c" { calls[$2] = $1; next }
    s == "d" { defs[$2] = $1; next }
    END {
      r = base
      n = 0
      for (f in encl) { c = calls[f] - defs[f]; v[f] = encl[f] * (c > 1 ? c : 1); r += v[f]; k[n++] = f }
      print r
      for (i = 1; i < n; i++) { t = k[i]; j = i - 1
        while (j >= 0 && k[j] > t) { k[j+1] = k[j]; j-- }
        k[j+1] = t }
      for (i = 0; i < n; i++) printf "%s=%d\n", k[i], v[k[i]]
    }')
  REACH=$(printf '%s\n' "$REACHMAP" | head -1)
  REACHFN=$(printf '%s\n' "$REACHMAP" | tail -n +2)
  AMBIG=$(printf '%s\n' "$DEFS" | awk '$1 > 1' | grep -c .)
fi

if [ "${1:-}" = "--update" ]; then
  mkdir -p "$(dirname "$BASELINE")"
  {
    echo "# untyped-alloc ratchet baseline (v7: production sites + one-level reach)"
    echo "# regenerate: scripts/untyped-alloc-ratchet.sh --update"
    echo "# see docs/known-issues/jdk-only/H0-6, H23-3 and WORKER-5-3"
    echo "# v6 counted 359 sites; 197 of them were inside a #[cfg(test)] mod of a"
    echo "# production file. The drop to $SITES is a CORRECTION, not a fix."
    echo "objects=$OBJ"
    echo "arrays=$ARR"
    echo "sites=$SITES"
    echo "test_sites=$TESTS"
    echo "reach=$REACH"
    echo "widths=$WIDTHS"
    echo "byfn=$BYFN"
    # One line per enclosing function, so a `git diff` of this file names WHICH
    # function moved. The total above can stay still while two entries here move
    # in opposite directions — measured 2026-08-22.
    echo "reachfn_begin"
    printf '%s\n' "$REACHFN"
    echo "reachfn_end"
  } > "$BASELINE"
  echo "baseline written: objects=$OBJ arrays=$ARR sites=$SITES reach=$REACH"
  echo "                  test_sites=$TESTS (excluded) widths=[$WIDTHS]"
  exit 0
fi

if [ "${1:-}" = "--selftest" ]; then
  selftest_run() { RATCHET_BASELINE="$1" bash "$0" 2>&1; }
  tmp=$(mktemp -d) || exit 3
  # shellcheck disable=SC2064
  trap "rm -rf '$tmp'" EXIT
  fails=0
  note() { echo "  $1"; }
  expect() { # <label> <expected-rc> <must-contain> <baseline-file>
    local out rc
    out=$(selftest_run "$4"); rc=$?
    if [ "$rc" -ne "$2" ]; then
      note "FAIL $1: rc=$rc expected $2"; fails=1; return
    fi
    if [ -n "$3" ] && ! printf '%s' "$out" | grep -q "$3"; then
      note "FAIL $1: output did not contain '$3'"; printf '%s\n' "$out" | sed 's/^/      /'
      fails=1; return
    fi
    note "ok   $1 (rc=$rc)"
  }
  mkbase() { # <file> <obj> <arr> <reach> <widths> <byfn>
    { echo "objects=$2"; echo "arrays=$3"; echo "reach=$4"
      echo "widths=$5"; echo "byfn=$6"; } > "$1"
  }

  echo "SELFTEST: exercising every failure path"
  note "measured now: objects=$OBJ arrays=$ARR reach=$REACH test_sites=$TESTS"

  # (0) The gate can match at all, and the two new numbers are sane.
  [ "$OBJ" -gt 0 ] || { note "FAIL object pattern matched nothing"; fails=1; }
  [ "$ARR" -gt 0 ] || { note "FAIL array pattern matched nothing"; fails=1; }
  [ "$TESTS" -gt 0 ] || { note "FAIL cfg(test) filter suppressed nothing — at"
                          note "     22cb4338d it suppresses 197 sites; a zero"
                          note "     means the block filter stopped matching."; fails=1; }
  [ "$REACH" -ge "$SITES" ] || { note "FAIL reach ($REACH) < sites ($SITES) — reach"
                                 note "     counts each site at least once."; fails=1; }
  case "$BYFN" in *alloc_object=*) ;; *) note "FAIL breakdown lost alloc_object"; fails=1;; esac
  case "$BYFN" in *class_is=*) note "note: class_is discovered (expected, not counted)";; esac

  # (1) each ratcheted column TRIPS on growth
  mkbase "$tmp/b1" "$((OBJ-1))" "$ARR" "$REACH" "$WIDTHS" "$BYFN"
  expect "object growth trips"  1 "object sites GREW"  "$tmp/b1"
  mkbase "$tmp/b2" "$OBJ" "$((ARR-1))" "$REACH" "$WIDTHS" "$BYFN"
  expect "array growth trips"   1 "array sites GREW"   "$tmp/b2"
  mkbase "$tmp/b3" "$OBJ" "$ARR" "$((REACH-1))" "$WIDTHS" "$BYFN"
  expect "reach growth trips"   1 "reach GREW"         "$tmp/b3"

  # (2) a NEW carrier width and a NEW spelling each trip
  mkbase "$tmp/b4" "$OBJ" "$ARR" "$REACH" "$(printf '%s' "$WIDTHS" | sed 's/^[0-9]* //')" "$BYFN"
  expect "new width trips"      1 "NEW carrier width"  "$tmp/b4"
  mkbase "$tmp/b5" "$OBJ" "$ARR" "$REACH" "$WIDTHS" "$(printf '%s' "$BYFN" | sed 's/^[^ ]* //')"
  expect "new spelling trips"   1 "NEW function"       "$tmp/b5"

  # (3) an improvement is REPORTED and does NOT trip
  mkbase "$tmp/b6" "$((OBJ+1))" "$ARR" "$REACH" "$WIDTHS" "$BYFN"
  expect "improvement reported" 0 "IMPROVED"           "$tmp/b6"

  # (4) the exact baseline passes
  mkbase "$tmp/b7" "$OBJ" "$ARR" "$REACH" "$WIDTHS" "$BYFN"
  expect "matching baseline ok" 0 "ok — no growth"     "$tmp/b7"

  # (5) a missing baseline is rc=2, not a silent pass
  expect "missing baseline"     2 "no baseline at"     "$tmp/nope"

  # (6) THE ZERO GUARD. v3 printed "ok" with rc=0 while matching nothing; the
  #     only way to know that cannot happen again is to make it happen.
  out=$(RATCHET_CRATES="scripts/baselines" RATCHET_BASELINE="$tmp/b7" bash "$0" 2>&1); rc=$?
  if [ "$rc" -eq 3 ] && printf '%s' "$out" | grep -q "matched ZERO"; then
    note "ok   zero-match guard (rc=3)"
  else
    note "FAIL zero-match guard: rc=$rc"; printf '%s\n' "$out" | sed 's/^/      /'; fails=1
  fi

  [ "$fails" -eq 0 ] && { echo "  selftest OK — all nine paths fire"; exit 0; }
  exit 3
fi

[ -f "$BASELINE" ] || { echo "UNTYPED-ALLOC RATCHET: no baseline at $BASELINE"
  echo "  measured now: objects=$OBJ arrays=$ARR sites=$SITES reach=$REACH"
  echo "                test_sites=$TESTS widths=[$WIDTHS]"
  echo "  run: scripts/untyped-alloc-ratchet.sh --update"; exit 2; }

B_OBJ=$(sed -n 's/^objects=//p' "$BASELINE"); B_OBJ=${B_OBJ:-0}
B_ARR=$(sed -n 's/^arrays=//p' "$BASELINE");  B_ARR=${B_ARR:-0}
B_REACH=$(sed -n 's/^reach=//p' "$BASELINE"); B_REACH=${B_REACH:-0}
B_WIDTHS=$(sed -n 's/^widths=//p' "$BASELINE")
B_BYFN=$(sed -n 's/^byfn=//p' "$BASELINE")
B_REACHFN=$(sed -n '/^reachfn_begin$/,/^reachfn_end$/p' "$BASELINE" | sed '1d;$d')
bad=0

echo "UNTYPED-ALLOC RATCHET (v7)"
echo "  objects: $OBJ (baseline $B_OBJ)      <- fabricated carriers"
echo "  arrays : $ARR (baseline $B_ARR)      <- untyped component class"
echo "  reach  : $REACH (baseline $B_REACH)      <- sites weighted by callers of the enclosing fn"
echo "  widths : [$WIDTHS] (baseline [$B_WIDTHS])"
echo "  by fn  : $BYFN"
echo "  excluded: $TESTS site(s) inside a #[cfg(test)] mod of a production file"
echo "  LIMITS (read these before quoting any number above):"
echo "    L1 reach follows ONE level of call graph. Both columns are FLOORS."
echo "    L2 attribution is by function NAME; $AMBIG name(s) have >1 definition."
echo "    L3 grep over source text — a cfg-gated or macro-generated site is invisible."
echo "       CRATONVM_DBG_ANONALLOC=1 is the runtime instrument; neither alone is"
echo "       authoritative (H0-6 §8 measured it attributing 4.4% of events)."
echo "    L4 this ratchets DRIFT. It is not the size of the problem."

for triple in "object:$OBJ:$B_OBJ" "array:$ARR:$B_ARR" "reach:$REACH:$B_REACH"; do
  k=${triple%%:*}; rest=${triple#*:}; now=${rest%%:*}; was=${rest#*:}
  if [ "$now" -gt "$was" ]; then
    if [ "$k" = reach ]; then
      echo "  TRIPPED: reach GREW by $((now - was))."
      echo "    Either a new site, or a new CALLER of a helper that already"
      echo "    fabricates. The second kind is invisible to the site columns —"
      echo "    that is the whole reason this column exists."
    else
      echo "  TRIPPED: $k sites GREW by $((now - was))."
      echo "    Each is a caller that resolved a class, failed, and hands out the"
      echo "    wrong type. If deliberate, say why in the commit and re-baseline."
    fi
    bad=1
  elif [ "$now" -lt "$was" ]; then
    echo "  IMPROVED: $k down $((was - now)) — re-baseline so the gain holds."
  fi
done

# Per-function reach movement, reported ALWAYS — including when the total did
# not move. Two entries moving in opposite directions is not a hypothetical:
# see the comment on REACHMAP above.
if [ -n "${REACHFN:-}" ] && [ -n "$B_REACHFN" ]; then
  MOVED=$(printf 'B\n%s\nN\n%s\n' "$B_REACHFN" "$REACHFN" | awk '
    /^B$/ { s = "b"; next }
    /^N$/ { s = "n"; next }
    NF == 0 { next }
    { split($0, a, "="); if (s == "b") b[a[1]] = a[2]; else n[a[1]] = a[2]; seen[a[1]] = 1 }
    END { for (f in seen) if (b[f] + 0 != n[f] + 0)
            printf "    %-44s %4d -> %-4d\n", f, b[f], n[f] }' | sort)
  if [ -n "$MOVED" ]; then
    echo "  reach MOVED per function (a still TOTAL can hide an offsetting pair):"
    printf '%s\n' "$MOVED"
  fi
fi

for w in $WIDTHS; do
  case " $B_WIDTHS " in *" $w "*) ;;
    *) echo "  TRIPPED: NEW carrier width $w — a shape no baseline has seen."
       echo "    Widths are families: width 4 is the HashMap.Node (H0-6 §7)."; bad=1;; esac
done

for kv in $BYFN; do
  fn="${kv%%=*}"
  case " $B_BYFN " in *" $fn="*) ;;
    *) echo "  TRIPPED: NEW function '$fn' takes the sentinel — no baseline has it."
       echo "    Classify it: allocator (add to the allowlist) or not (document why)."
       echo "    Two earlier versions of this gate were wrong by exactly this."; bad=1;; esac
done

[ "$bad" -eq 0 ] && echo "  ok — no growth, no new widths, no new spellings."
exit "$bad"
