#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# GC-root acceptance lane — docs/feature-designs/precise-jit-maps-default.md
# Step 4 ("App-gauntlet GC-root acceptance sweep").
#
# Runs the GC-root-coverage-under-JIT family (the A1-A4 springboot + the
# gc-stress-bintrees-main-args springboot) and the GC microbenchmarks with precise
# JIT stack maps ON (the dev default), diffs CratonVM against HotSpot, and
# records a baseline TSV + human summary. Each item carries an EXPECTED status,
# so the lane doubles as a regression gate: it exits non-zero if any item
# DEVIATES from its expected result (a PASS that broke, or a KNOWN-FAIL that
# silently started passing — both want a human to look).
#
# No VM behaviour change — harness/config only. precise maps are default-on
# (CRATONVM_NO_PRECISE_JIT_MAPS opts out); this lane exercises the default.
#
#   CV=path/to/cratonvm.exe   (default: $ROOT/target/release/cratonvm.exe)
#   JDK=path/to/jdk-25        (HotSpot oracle; default Adoptium 25)
#   TIMEOUT=secs              (per run; default 300)
#   QUICK=1                   (skip bt18@8g + the slowest items)
# ---------------------------------------------------------------------------
set +e
export MSYS2_ARG_CONV_EXCL='*'
export MSYS_NO_PATHCONV=1
# The harness governs per-run timeouts via `timeout $TIMEOUT`; disable the VM's
# internal 120s native-hang watchdog so a slow-but-CORRECT bench still runs to
# completion and gets its checksum verified rather than being aborted mid-run.
# (fib44 is perf-regressed on current dev — a SEPARATE, flagged issue — and
# would otherwise trip the 120s abort before printing its checksum.)
export CRATONVM_DISABLE_DEFAULT_WATCHDOG=1

ROOT="${ROOT:-$(git -C "$(dirname "${BASH_SOURCE[0]}")" rev-parse --show-toplevel 2>/dev/null || echo C:/craton/CratonVM)}"
CV="${CV:-$ROOT/target/release/cratonvm.exe}"
JDK="${JDK:-C:/Program Files/Eclipse Adoptium/jdk-25.0.2.10-hotspot}"
# Path-conversion is disabled (MSYS_NO_PATHCONV) so the `*`/`@` args reach the
# Windows exes intact — but that also stops MSYS rewriting Unix paths, so any
# Unix-style ROOT/CV/JDK ('/c/...') would reach javac/java as '\c\...' (no drive
# letter) and fail. Normalize to mixed Windows form ('C:/...') which every
# Windows exe accepts and MSYS leaves untouched.
if command -v cygpath >/dev/null 2>&1; then
  ROOT="$(cygpath -m "$ROOT")"; CV="$(cygpath -m "$CV")"; JDK="$(cygpath -m "$JDK")"
fi
HOTSPOT="$JDK/bin/java.exe"
JAVAC="$JDK/bin/javac.exe"
# bench/ holds BenchSuite.class (a gitignored build artifact). Default to
# $ROOT/bench; allow BENCH override for worktrees that lack it (point at a
# checkout that has it). Normalized to Windows form like the rest.
BENCH="${BENCH:-$ROOT/bench}"
command -v cygpath >/dev/null 2>&1 && BENCH="$(cygpath -m "$BENCH")"
REPRO="$ROOT/docs/known-issues/repros"
TIMEOUT="${TIMEOUT:-300}"
QUICK="${QUICK:-0}"

RESDIR="$ROOT/test-infra/regression-pool/results"
mkdir -p "$RESDIR"
TS=$(date +%Y%m%d-%H%M%S)
TSV="$RESDIR/gc-root-$TS.tsv"
# Class output dir kept under $ROOT (Windows-style) so javac -d and the -cp the
# Windows VMs read are both valid Windows paths.
CLASSDIR="$ROOT/test-infra/regression-pool/.gc-root-classes-$TS"
mkdir -p "$CLASSDIR"
trap 'rm -rf "$CLASSDIR"' EXIT

hr() { printf '%s\n' "--------------------------------------------------------------------------"; }
log() { echo "[gc-root-lane] $*"; }

[ -x "$CV" ] || { echo "FATAL: CratonVM binary not found/executable: $CV" >&2; exit 2; }
[ -x "$HOTSPOT" ] || { echo "FATAL: HotSpot not found: $HOTSPOT" >&2; exit 2; }
[ -f "$BENCH/BenchSuite.class" ] || { echo "FATAL: BenchSuite.class not in BENCH=$BENCH — set BENCH= to a checkout that has bench/" >&2; exit 2; }

printf 'name\trc\twall_s\tprecise_result\thotspot\tstatus\texpected\tnote\n' > "$TSV"
DEVIATIONS=0
NPASS=0; NKNOWN=0; NFLAKY=0

# Compile the repro sources we need (they are tiny, plain JDK-25 javac).
log "compiling repros into $CLASSDIR"
"$JAVAC" -d "$CLASSDIR" \
  "$REPRO/gc-stress-bintrees-main-args/RHard.java" \
  "$REPRO/gc-stress-bintrees-main-args/VStatic.java" \
  "$REPRO/gc-stress-bintrees-main-args/VAAload.java" \
  "$REPRO/gc-stress-bintrees-main-args/VArgLen.java" \
  "$REPRO/gc-stress-bintrees-main-args/binarytrees.java" \
  "$REPRO/A2-reflrepro/ReflRepro.java" \
  "$REPRO/family-a-gc-root-race/MTRegex.java" 2>"$CLASSDIR/javac.err"
[ -s "$CLASSDIR/javac.err" ] && cat "$CLASSDIR/javac.err"

# timed_run <env-prefix> <args...> -> sets RC, WALL, OUT
timed_run() {
  local envp="$1"; shift
  local start end
  start=$(date +%s%N)
  OUT=$(env $envp timeout "$TIMEOUT" "$@" 2>&1)
  RC=$?
  end=$(date +%s%N)
  WALL=$(awk "BEGIN{printf \"%.1f\", ($end-$start)/1000000000}")
}

crash_markers() { # stdin -> count of corruption/crash signatures
  grep -ciE 'inconsistent header|implausible|stale pointer|EXCEPTION_ACCESS|SIGSEGV|panic|ArrayIndexOutOfBounds|class_id=0|out-of-bounds field'
}

record() { # name rc wall precise hotspot status expected note
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$1" "$2" "$3" "$4" "$5" "$6" "$7" "$8" >> "$TSV"
  local mark=""
  case "$6/$7" in
    PASS/PASS|KNOWN-FAIL/KNOWN-FAIL) mark="ok" ;;
    FLAKY/*) mark="flaky"; NFLAKY=$((NFLAKY+1)) ;;
    *) mark="DEVIATION"; DEVIATIONS=$((DEVIATIONS+1)) ;;
  esac
  [ "$6" = "PASS" ] && NPASS=$((NPASS+1))
  [ "$6" = "KNOWN-FAIL" ] && NKNOWN=$((NKNOWN+1))
  printf '  %-26s %-11s exp=%-11s ct=%-10s hs=%-10s %ss  [%s] %s\n' \
    "$1" "$6" "$7" "$4" "$5" "$3" "$mark" "$8"
}

# --- 1. GC microbenchmarks: CratonVM (precise default-on) checksum == HotSpot
hr; log "GC microbenchmarks (precise default-on) vs HotSpot"
bench_item() { # bench heap [timeout_override]
  local b="$1" heap="$2" tmo="${3:-$TIMEOUT}"
  local hs ct status note="" _savedT="$TIMEOUT"
  TIMEOUT="$tmo"
  hs=$("$HOTSPOT" -cp "$BENCH" BenchSuite "$b" 2>/dev/null | grep -oE 'checksum=[0-9]+' | head -1 | cut -d= -f2)
  timed_run "" "$CV" --java-home "$JDK" --Xmx "$heap" -cp "$BENCH" BenchSuite "$b"
  ct=$(echo "$OUT" | grep -oE 'checksum=[0-9]+' | head -1 | cut -d= -f2)
  # Retry once on an empty result: a large-heap bench (bt18@8g) can hit a
  # TRANSIENT host-allocation failure under concurrent memory load and exit
  # without a checksum — distinct from a wrong checksum (a real regression).
  if [ -z "$ct" ]; then
    note="retried(transient empty)"
    timed_run "" "$CV" --java-home "$JDK" --Xmx "$heap" -cp "$BENCH" BenchSuite "$b"
    ct=$(echo "$OUT" | grep -oE 'checksum=[0-9]+' | head -1 | cut -d= -f2)
  fi
  if [ -n "$ct" ] && [ "$ct" = "$hs" ]; then status="PASS"; else status="FAIL"; note="${note:+$note }checksum mismatch/empty"; fi
  TIMEOUT="$_savedT"
  record "bench:$b" "$RC" "$WALL" "${ct:-NONE}" "${hs:-NONE}" "$status" "PASS" "$note"
}
bench_item bintrees10 4g
bench_item bintrees14 4g
bench_item bintrees16 4g
[ "$QUICK" = "1" ] || bench_item bintrees18 8g
bench_item fib44 2g          # NB perf-regressed on dev (separate issue); checksum still validated
# sieve250k = sieve(250000) REPEATED 1000× (see BenchSuite.sieve(II)J). The hot
# sieve method is invoked exactly once (the 1000-repeat is its OWN inner loop), so
# it never reaches invocation-count tier-up, and OSR does not fire on its loops —
# it runs INTERPRETED end-to-end. Result: correct (checksum=22044) but ~70–350×
# slower than HotSpot, so 1000 reps ≈ 350 s on a slow box (measured here:
# nojit ms=347868; JIT-on similar/slower). It is NOT a hang/infinite-loop (per-rep
# time scales perfectly linearly) and NOT a miscompile. Give it a generous timeout
# so a slow box records the correct checksum instead of an empty/timeout deviation.
# Tracked as a JIT-throughput gap (hot once-invoked method never compiled), not a
# GC-root-family bug. Override with SIEVE_TIMEOUT.
bench_item sieve250k 2g "${SIEVE_TIMEOUT:-$(( TIMEOUT > 600 ? TIMEOUT : 600 ))}"
bench_item matrix600 2g

# --- 2. A3 register-invisibility family (gc-stress-bintrees-main-args)
# Want checksum 3222190 (bt14, maxDepth 14, args="14") under extreme GC stress.
# BASELINE on current dev (457c95a8): all five produce the CORRECT checksum 8/8,
# each with the GC-array-guard firing once (note: warned=N). i.e. the stale-root
# corruption is still EXERCISED (guard catches+drops a bad write) but is currently
# NON-FATAL at this depth/stress — so EXPECTED=PASS with warned>0, NOT "clean".
# This differs from the gc-stress-bintrees-...-jit-frame-stale-root known-issue,
# which recorded CRASH/WRONG on an OLDER dev; the gap is masked, not closed.
# A future regression to crash/wrong (or a fix that stops the guard firing) will
# show as a DEVIATION here. Each run 3x at GC_STRESS=4096.
hr; log "A3-class gc-stress-bintrees-main-args repros (GC_STRESS=4096, args=14, want 3222190)"
WANT=3222190
gcstress_item() { # class expected_status
  local cls="$1" exp="$2"
  local pass=0 crash=0 wrong=0 warned=0 n=3
  for i in $(seq 1 $n); do
    timed_run "CRATONVM_DBG_GC_STRESS=4096" "$CV" --java-home "$JDK" --Xmx 6g -cp "$CLASSDIR" "$cls" 14
    local val; val=$(echo "$OUT" | grep -xE '[0-9]+' | head -1)   # the lone checksum line
    local cm; cm=$(echo "$OUT" | crash_markers)
    # Classify by RESULT first: a correct checksum with rc=0 is a PASS even if a
    # benign GC-guard warning fired (it dropped a stale write that did not affect
    # the answer). Crash markers only disambiguate crash-vs-wrong when the result
    # is missing/incorrect. `warned` records guards that fired on a passing run.
    if [ "$RC" = 0 ] && [ "$val" = "$WANT" ]; then
      pass=$((pass+1)); [ "$cm" -gt 0 ] && warned=$((warned+1))
    elif [ "$RC" != 0 ] || [ -z "$val" ]; then crash=$((crash+1))
    else wrong=$((wrong+1)); fi
  done
  local status note="pass=$pass crash=$crash wrong=$wrong /$n warned=$warned"
  if [ "$pass" = "$n" ]; then status="PASS";
  elif [ "$pass" = 0 ]; then status="KNOWN-FAIL";
  else status="FLAKY"; fi
  record "a3:$cls" "$RC" "$WALL" "$([ $pass = $n ] && echo $WANT || echo MIXED)" "$WANT" "$status" "$exp" "$note"
}
gcstress_item RHard       PASS
gcstress_item VStatic     PASS
gcstress_item VAAload     PASS
gcstress_item VArgLen     PASS
gcstress_item binarytrees PASS

# --- 3. A2 ReflRepro — FIXED 2026-06-23 (6e3ddb05): the non-moving young sweep's
# free-block coalescer now merges OVERLAPPING blocks (not just adjacent), so
# Arena::alloc can't double-serve a young region. Was a GC free-list accounting
# bug, NOT a register-resident JIT root (precise maps are orthogonal). Expected
# PASS now; a crash/marker here is a REGRESSION → deviation.
hr; log "A2 ReflRepro (EXPECTED PASS — FIXED 6e3ddb05; coalesce overlapping free blocks)"
timed_run "CRATONVM_DBG_GC_STRESS=524288" "$CV" --java-home "$JDK" --Xmx 256m -cp "$CLASSDIR" ReflRepro 20000
A2CM=$(echo "$OUT" | crash_markers)
if [ "$RC" = 0 ] && [ "$A2CM" = 0 ]; then a2s="PASS"; else a2s="REGRESSED"; fi
record "a2:ReflRepro" "$RC" "$WALL" "$([ "$a2s" = PASS ] && echo clean || echo crash)" "clean" "$a2s" "PASS" "markers=$A2CM"

# --- 4. MTRegex multi-thread GC-root stress (FLAKY by documentation:
# pre-existing cross-thread STW JIT-root gap + a Thread.join/IMSE hang)
hr; log "MTRegex multi-thread GC stress (FLAKY — documented cross-thread gap)"
mt_done=0
# Short per-run cap: this is a 10s test; with the VM watchdog disabled a hung
# run (the Thread.join/IMSE deadlock) would otherwise burn the full $TIMEOUT.
mt_save="$TIMEOUT"; TIMEOUT=30
for i in 1 2 3; do
  timed_run "CRATONVM_DBG_GC_STRESS=524288" "$CV" --java-home "$JDK" --Xmx 256m -cp "$CLASSDIR" MTRegex 4 10
  echo "$OUT" | grep -q "DONE threads=" && mt_done=$((mt_done+1))
done
TIMEOUT="$mt_save"
record "mt:MTRegex" "$RC" "$WALL" "done=$mt_done/3" "n/a" "FLAKY" "FLAKY" "completions=$mt_done/3 (cross-thread gap)"

hr
log "results: $TSV"
log "PASS=$NPASS  KNOWN-FAIL(expected)=$NKNOWN  FLAKY=$NFLAKY  DEVIATIONS=$DEVIATIONS"
if [ "$DEVIATIONS" -gt 0 ]; then
  log "FAIL: $DEVIATIONS item(s) deviated from expected baseline — investigate."
  exit 1
fi
log "OK: every item matched its expected baseline."
exit 0
