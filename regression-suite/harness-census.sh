#!/usr/bin/env bash
# The regression suite's JDK-ONLY CENSUS ARITHMETIC.
#
# WHY THIS FILE EXISTS
# --------------------
# Two defects in the census `run.sh` prints after a `--jdk-only` sweep. Both are
# in the arithmetic, not in the VM, and both make the run report MORE certainty
# than it has.
#
# ---------------------------------------------------------------------------
# 1. `sort -u` OVER WHOLE JSON LINES IS NOT A UNION OVER TRIPLES
# ---------------------------------------------------------------------------
# `run.sh` unions the per-vector reports with a whole-line `sort -u` and counts
# outcomes with `grep -c`. Its comment argued this is exact:
#
#   "Rows are byte-identical across vectors for the same fact — `summary` is a
#    pure function of the other fields — so `sort -u` is a real UNION and not an
#    approximation."
#
# `summary` IS a pure function of the other fields. The premise fails one field
# earlier: **`native_kind` and `outcome` are properties of a DISPATCH, not of a
# triple.** The same triple dispatches differently in different vectors, so
# `sort -u` unions `(triple, native_kind, outcome)` and the same method is
# counted more than once.
#
# MEASURED 2026-08-21, `SUITE=all CRATONVM_ARGS=--jdk-only`, 105 reports,
# cratonvm-r10.exe:
#
#     whole-line distinct rows          1868
#     distinct TRIPLES                  1457
#     triples appearing more than once   387   (all differ in native_kind,
#                                               385 also differ in outcome)
#
#     native-won     run.sh 1387   triples 1387   double-counted   0
#     bytecode-won   run.sh  481   triples  455   double-counted  26
#
# And the part that matters more than the double-count:
#
#     native-won triples          1387
#     bytecode-won triples         455
#     in BOTH                      385
#     bytecode-won and NEVER native 70
#
# `run.sh` labels its 481 **"bytecode-won (the contract working)"**. The contract
# demonstrably worked, for a triple that never also ran native, **70 times** —
# not 481. The other 385 ran the native somewhere in the same suite, which is the
# defect the census exists to count. A 6.9x overstatement of the good news.
#
# ---------------------------------------------------------------------------
# 2. THE SATURATION GREP CANNOT MATCH THE ONE SINK THAT IS UNMEASURED
# ---------------------------------------------------------------------------
# `run.sh` asks `grep -l '"truncated": true'` and, finding none, prints
#
#   "saturation: none — no report truncated a bounded collection, so the counts
#    above are totals, not floors."
#
# The report has THREE bounded collections. The third, `jit_compile`, lives in
# `cratonvm_jit` and has no counter, so `vm_init.rs` renders it
# `"truncated": null` — deliberately, because *"an unmeasured thing must not
# render as a clean one"*. The `= true` grep matches neither `false` nor `null`,
# so the harness converts a `null` into a positive claim of completeness.
#
# MEASURED on the same 105 reports: **every one carries a `"truncated": null`**
# (54 `false` + 27 `null` across the partial set inspected; one `null` per file).
# So the "totals, not floors" line has been printed over an unmeasured sink on
# every strict run there has ever been.
#
# The fix is not to guess: it is a THIRD verdict. `true` -> saturated.
# All-`false` -> totals. Any `null` -> **UNKNOWN**, and say which sink.
#
# Usage:  . regression-suite/harness-census.sh
#         census_shadow_summary < deduped-rows      # -> a `k=v` line
#         census_saturation <reportdir>             # -> verdict lines
#         regression-suite/harness-census.sh --selftest
# ---------------------------------------------------------------------------

# Read deduplicated `native-shadows-bytecode` rows on stdin; print one line of
# `k=v` pairs. Counting is by TRIPLE (class+method+descriptor), which is the
# unit every record in docs/known-issues/jdk-only/ quotes.
#
# awk, not grep -c: the whole point is that the answer depends on fields the
# line-level tools cannot separate.
census_shadow_summary() {
  awk '
    function field(line, key,   m) {
      if (!match(line, "\"" key "\":\"[^\"]*\"")) return ""
      m = substr(line, RSTART, RLENGTH)
      sub("^\"" key "\":\"", "", m); sub("\"$", "", m)
      return m
    }
    {
      lines++
      t = field($0, "class") "\t" field($0, "method") "\t" field($0, "descriptor")
      o = field($0, "outcome")
      if (o == "native-won")   nw[t] = 1
      if (o == "bytecode-won") bw[t] = 1
      seen[t] = 1
    }
    END {
      for (t in bw) { bwn++; if (t in nw) both++ }
      for (t in nw) nwn++
      for (t in seen) tot++
      printf "lines=%d triples=%d native=%d bytecode=%d both=%d bytecode_only=%d\n",
             lines+0, tot+0, nwn+0, bwn+0, both+0, bwn+0 - both+0
    }'
}

# The saturation verdict for a directory of `--jdk-only-report` files.
# Prints the operator-facing lines; returns 0 when the counts are totals, 1 when
# they are floors or when completeness is UNKNOWN.
census_saturation() {
  _cs_dir="$1"
  _cs_true=$(grep -l '"truncated": true' "$_cs_dir"/*.json 2>/dev/null | grep -c .)
  # `null` is emitted by a sink that has no counter yet. It is NOT `false`, and
  # treating the absence of `true` as `false` is what turned an unmeasured sink
  # into a clean bill of health.
  _cs_null=$(grep -l '"truncated": null' "$_cs_dir"/*.json 2>/dev/null | grep -c .)
  _cs_drop=$(grep -h '"dropped": ' "$_cs_dir"/*.json 2>/dev/null | grep -v null \
               | sed 's/[^0-9]//g' | awk '{s+=$1} END {print s+0}')
  if [ "$_cs_true" -gt 0 ]; then
    echo "  SATURATED: $_cs_true report(s) truncated a bounded collection and dropped ~$_cs_drop row(s)."
    echo "    EVERY count above is a FLOOR. Re-run with CRATONVM_NATIVE_SHADOW_SINK_CAP=<bigger>."
    [ "$_cs_null" -gt 0 ] && echo "    ALSO: $_cs_null report(s) carry an UNMEASURED sink (see below)."
    return 1
  fi
  if [ "$_cs_null" -gt 0 ]; then
    echo "  saturation: UNKNOWN — no report says \`truncated: true\`, but $_cs_null carry"
    echo "    \`\"truncated\": null\`: the report's third bounded collection (jit_compile,"
    echo "    in cratonvm_jit) has no counter, so whether it overflowed is UNMEASURED."
    echo "    The counts above are NOT known to be totals. vm_init.rs renders that sink"
    echo "    null on purpose — an unmeasured thing must not render as a clean one — and"
    echo "    until 2026-08-21 this line read \"saturation: none … totals, not floors\"."
    return 1
  fi
  echo "  saturation: none — every bounded collection reported \`truncated: false\`, so the"
  echo "    counts above are totals, not floors."
  return 0
}

# ---------------------------------------------------------------------------
if [ "${1:-}" = "--selftest" ]; then
  _f=0
  _ok() { printf '  ok   %s\n' "$1"; }
  _no() { printf '  FAIL %s\n' "$1"; _f=1; }
  row() { # <class> <method> <desc> <kind> <outcome>
    printf '{"kind":"native-shadows-bytecode","summary":"%s native shadows bytecode of %s.%s%s [%s]","class":"%s","method":"%s","descriptor":"%s","native_kind":"%s","outcome":"%s"}\n' \
      "$4" "$1" "$2" "$3" "$5" "$1" "$2" "$3" "$4" "$5"
  }
  echo "SELFTEST harness-census.sh"

  # (1) THE DEFECT: one triple, two dispatch shapes -> two lines, ONE triple.
  got=$( { row A m '()V' bridge native-won
           row A m '()V' bridge-ran-over-bytecode bytecode-won; } | census_shadow_summary)
  case "$got" in
    "lines=2 triples=1 native=1 bytecode=1 both=1 bytecode_only=0")
      _ok "one triple with two outcomes counts as ONE triple, in BOTH buckets" ;;
    *) _no "two-shape triple: got [$got]" ;;
  esac

  # (2) a triple that is ONLY ever bytecode-won is the "contract working" unit.
  got=$( { row A m '()V' bridge native-won
           row B n '(I)V' bridge bytecode-won; } | census_shadow_summary)
  case "$got" in
    "lines=2 triples=2 native=1 bytecode=1 both=0 bytecode_only=1")
      _ok "a never-native triple is counted as bytecode_only" ;;
    *) _no "disjoint triples: got [$got]" ;;
  esac

  # (3) the same triple from two vectors with the SAME shape is one line after
  #     sort -u and must stay one triple.
  got=$( { row A m '()V' bridge native-won; } | census_shadow_summary)
  case "$got" in
    "lines=1 triples=1 native=1 bytecode=0 both=0 bytecode_only=0") _ok "single row" ;;
    *) _no "single row: got [$got]" ;;
  esac

  # (4) empty input must not report anything as complete.
  got=$(printf '' | census_shadow_summary)
  case "$got" in
    "lines=0 triples=0 native=0 bytecode=0 both=0 bytecode_only=0") _ok "empty input is all zeros" ;;
    *) _no "empty input: got [$got]" ;;
  esac

  # (5) descriptors carrying '/', ';' and '[' must survive field extraction.
  got=$( { row 'java/util/Map' get '(Ljava/lang/Object;)Ljava/lang/Object;' bridge native-won
           row 'java/util/Map' toArray '()[Ljava/lang/Object;' bridge native-won; } \
         | census_shadow_summary)
  case "$got" in
    "lines=2 triples=2 native=2 bytecode=0 both=0 bytecode_only=0")
      _ok "descriptors with / ; and [ parse as distinct triples" ;;
    *) _no "descriptor parsing: got [$got]" ;;
  esac

  # ---- census_saturation, all three verdicts + the empty dir ----------------
  d=$(mktemp -d) || exit 3
  # shellcheck disable=SC2064
  trap "rm -rf '$d'" EXIT

  mk() { printf '{"observation_sink": { "truncated": %s, "dropped": %s }}\n' "$2" "$3" > "$d/$1.json"; }

  rm -f "$d"/*.json; mk a false 0; mk b false 0
  out=$(census_saturation "$d"); rc=$?
  { [ "$rc" -eq 0 ] && printf '%s' "$out" | grep -q 'saturation: none'; } \
    && _ok "all-false -> totals (rc=0)" || _no "all-false: rc=$rc [$out]"

  rm -f "$d"/*.json; mk a false 0; mk b null null
  out=$(census_saturation "$d"); rc=$?
  { [ "$rc" -eq 1 ] && printf '%s' "$out" | grep -q 'saturation: UNKNOWN'; } \
    && _ok "a null sink -> UNKNOWN (rc=1), not 'none'" || _no "null: rc=$rc [$out]"

  rm -f "$d"/*.json; mk a true 17; mk b null null
  out=$(census_saturation "$d"); rc=$?
  { [ "$rc" -eq 1 ] && printf '%s' "$out" | grep -q 'SATURATED' \
      && printf '%s' "$out" | grep -q 'UNMEASURED sink'; } \
    && _ok "true wins over null, and the null is still reported" || _no "true+null: rc=$rc [$out]"

  rm -f "$d"/*.json
  out=$(census_saturation "$d"); rc=$?
  { [ "$rc" -eq 0 ] && printf '%s' "$out" | grep -q 'saturation: none'; } \
    && _ok "an EMPTY dir reports none (run.sh guards on \$jr_found separately)" \
    || _no "empty dir: rc=$rc [$out]"

  # (6) THE REGRESSION CHECK: the OLD one-line grep must still be blind to a
  #     null-only set. If a future VM starts rendering that sink `false`, this
  #     goes red and the UNKNOWN branch can be retired deliberately.
  rm -f "$d"/*.json; mk a false 0; mk b null null
  if [ "$(grep -l '"truncated": true' "$d"/*.json 2>/dev/null | grep -c .)" -eq 0 ]; then
    _ok "the premise holds: \`= true\` finds nothing in a null-carrying set"
  else
    _no "premise broken — re-read this file"
  fi

  [ "$_f" -eq 0 ] && { echo "  selftest OK — 5 counting cases, 4 saturation verdicts, the premise"; exit 0; }
  exit 3
fi
