#!/usr/bin/env bash
# Drive the full Spring Boot suite against ONE CratonVM build under ONE
# `--XX:UseGc` collector, one process per class via SbRunner.
#
# This is the Linux/azure counterpart of `run-spring-boot-suite.ps1`, and it
# exists as a tracked file for one reason: the ad-hoc copy that ran the
# 2026-08-19 three-collector sweep re-implemented the launch but dropped the
# per-class timeout table the PowerShell runner has carried since 2026-07-17.
# Every class whose validated wall exceeds the 300-second default was then
# reported as a HANG. In the 2026-08-21 re-run that was **eight of the nine
# HANG rows** -- all nine pass, with test counts identical to HotSpot, when
# each is given the budget the shared table already records for it.
#
# So: the budget table lives in `.suite/class-timeouts.tsv`, this script and
# `Get-EffectiveClassTimeoutSec` both read it, and a HANG row now records the
# budget it hit so "it timed out" and "it is stuck" stay distinguishable.
#
#   usage: run-sb-gc-shard.sh <Generational|G1|ZGC> <outdir> [parallel] [base_timeout]
#
# Environment:
#   CRATONVM_BIN   the cratonvm binary to test        (required)
#   SB_ROOT        the spring-boot fixture root       (default /data/cratonvm/apps/spring-boot)
#   JDK            a JDK 25 home                      (default /data/toolchain/jdk-25)
#   ALLTESTS       module<TAB>class index             (default .suite/all-tests.tsv)
#   MAX_HEAP       -Xmx for each class process        (default 2g)
set -u
set -o pipefail

GC="${1:?usage: run-sb-gc-shard.sh <Generational|G1|ZGC> <outdir> [parallel] [base_timeout]}"
OUT="${2:?outdir required}"
PARALLEL="${3:-3}"
BASE_TO="${4:-300}"

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CV="${CRATONVM_BIN:?CRATONVM_BIN must name the cratonvm binary under test}"
JDK="${JDK:-/data/toolchain/jdk-25}"
SB_ROOT="${SB_ROOT:-/data/cratonvm/apps/spring-boot}"
RUNNER_DIR="$SB_ROOT/sb-runner"
ALLTESTS="${ALLTESTS:-$HERE/.suite/all-tests.tsv}"
TIMEOUTS="${TIMEOUTS:-$HERE/.suite/class-timeouts.tsv}"
MAX_HEAP="${MAX_HEAP:-2g}"

mkdir -p "$OUT/logs"
RES="$OUT/results.tsv"
printf 'idx\tmodule\tclass\tstatus\trc\tms\ttimeout_s\tnote\n' > "$RES"

# A contended host inverts these results: every class is slower, and a fixed
# per-class budget turns that into HANG rows. Say so at the top of the log
# rather than leaving it to be reconstructed from the wall times afterwards.
echo "[$GC] host load at start: $(cut -d' ' -f1-3 /proc/loadavg), nproc=$(nproc), parallel=$PARALLEL"

# ── the shared per-class budget table ────────────────────────────────────────
# `module<TAB>class<TAB>seconds<TAB>why`. Missing file = every class gets the
# base default, which is the pre-2026-08-21 behaviour; say so loudly, because a
# silently-absent table is exactly the failure this script was written to stop.
declare -A CLASS_TIMEOUT=()
if [ -s "$TIMEOUTS" ]; then
  while IFS=$'\t' read -r m c s _why; do
    [ "$m" = "module" ] && continue
    [ -z "${c:-}" ] && continue
    CLASS_TIMEOUT["$m|$c"]="$s"
  done < <(tail -n +2 "$TIMEOUTS")
  echo "[$GC] per-class budgets loaded: ${#CLASS_TIMEOUT[@]} from $TIMEOUTS"
else
  echo "[$GC] WARNING: no per-class budget table at $TIMEOUTS — every class gets ${BASE_TO}s." >&2
  echo "[$GC] WARNING: slow-but-healthy classes will be reported as HANG. See this script's header." >&2
fi

effective_timeout() { # module class
  local want="${CLASS_TIMEOUT["$1|$2"]:-0}"
  if [ "$want" -gt "$BASE_TO" ]; then echo "$want"; else echo "$BASE_TO"; fi
}

GRADLE_USER_HOME="${GRADLE_USER_HOME:-/data/toolchain/gradle-home}"
GRADLE_JUNIT_CACHE="$GRADLE_USER_HOME/caches/modules-2/files-2.1"

crash_pattern='EXCEPTION_ACCESS_VIOLATION|SIGSEGV|SIGABRT|fatal runtime error|panicked at|thread .* panicked|internal error:|not yet implemented|illegal instruction|caught fatal signal|cratonvm panic|entered unreachable|core dumped'

CPCACHE_DIR="$OUT/.cpcache"
mkdir -p "$CPCACHE_DIR"

# A module's own testRuntimeClasspath usually already carries the launcher jar.
# Only add an external one when it does not, and then pick the SAME version the
# module already resolved for junit-platform-engine/junit-jupiter-engine --
# mixing versions throws NoSuchMethodError before any real test runs.
cp_for_module() {
  local module="$1" safemod cachefile cpfile mcp extra ver full
  safemod="$(printf '%s' "$module" | tr -c 'A-Za-z0-9_.' '_')"
  cachefile="$CPCACHE_DIR/$safemod"
  if [ -s "$cachefile" ]; then cat "$cachefile"; return; fi
  cpfile="$SB_ROOT/$module/build/cratonvm-test-cp.txt"
  mcp=""
  [ -s "$cpfile" ] && mcp="$(cat "$cpfile")"
  extra=""
  if ! printf '%s' "$mcp" | grep -q 'junit-platform-launcher'; then
    ver="$(printf '%s' "$mcp" | tr ':' '\n' | grep -oE 'junit-(platform-engine|jupiter-engine)-[0-9][0-9.]*\.jar' | head -1 | grep -oE '[0-9]+\.[0-9]+\.[0-9]+')"
    if [ -n "$ver" ]; then
      extra="$(find "$GRADLE_JUNIT_CACHE/org.junit.platform/junit-platform-launcher/$ver" -iname '*.jar' 2>/dev/null | grep -v sources | grep -v javadoc | head -1)"
    fi
    if [ -z "$extra" ]; then
      extra="$(find "$GRADLE_JUNIT_CACHE/org.junit.platform/junit-platform-launcher" -iname '*.jar' 2>/dev/null | grep -v sources | grep -v javadoc | sort -V | tail -1)"
    fi
    [ -n "$extra" ] && extra="$extra:"
  fi
  full="$RUNNER_DIR:${extra}${mcp}"
  printf '%s' "$full" > "$cachefile"
  echo "$full"
}

now_ms() { date +%s%3N; }
T0=$(date +%s)

run_one() {
  local idx="$1" module="$2" cls="$3" class_to="$4"
  local safe logf cp t_start t_end ms rc status note resline failed cfailed
  safe="$(printf '%s.%s' "$module" "$cls" | tr -c 'A-Za-z0-9_.' '_')"
  logf="$OUT/logs/$(printf '%05d' "$idx")-$safe.log"
  cp="$(cp_for_module "$module")"

  t_start="$(now_ms)"
  # cd into the module root first: several classes read fixtures with plain
  # relative paths, which only resolve when the JVM's cwd is the module
  # directory. Running from the wrong cwd threw FileNotFoundException for
  # ~30/91 of a full-suite shard's FAIL rows on 2026-08-19, none of them real.
  ( cd "$SB_ROOT/$module" && timeout --kill-after=5 "$class_to" "$CV" --java-home "$JDK" --Xmx "$MAX_HEAP" \
    --XX:UseGc "$GC" -cp "$cp" SbRunner "$cls" ) > "$logf" 2>&1
  rc=$?
  t_end="$(now_ms)"
  ms=$((t_end - t_start))

  status="FAIL"
  note=""
  if [ "$rc" -eq 124 ] || [ "$rc" -eq 137 ]; then
    status="HANG"
    # Record the budget that was hit. A HANG is a statement about this run's
    # timeout, not about the class: without the number a slow-but-healthy class
    # and a deadlocked one produce identical rows.
    note="hit the ${class_to}s budget"
  elif grep -qaiE "$crash_pattern" "$logf"; then
    status="CRASH"
    note="$(grep -aiE "$crash_pattern" "$logf" | head -1 | tr -d '\r\n' | cut -c1-160)"
  else
    resline="$(grep -a '^SBRUNNER_RESULT' "$logf" | tail -1)"
    if [ -n "$resline" ]; then
      failed=$(printf '%s' "$resline" | grep -oE 'failed=[0-9]+' | cut -d= -f2)
      cfailed=$(printf '%s' "$resline" | grep -oE 'containersFailed=[0-9]+' | cut -d= -f2)
      if [ "$rc" -eq 0 ] && [ "${failed:-1}" = "0" ] && [ "${cfailed:-0}" = "0" ]; then
        status="PASS"
      else
        status="FAIL"
        note="$resline"
      fi
    else
      note="no SBRUNNER_RESULT line, rc=$rc"
    fi
  fi

  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$idx" "$module" "$cls" "$status" "$rc" "$ms" "$class_to" "$note" >> "$RES"
  local elapsed=$(( $(date +%s) - T0 ))
  echo "[$GC][$elapsed s] ($idx) $status  ${ms}ms/${class_to}s  $module/$cls"
}
export -f run_one cp_for_module now_ms
export CV JDK RUNNER_DIR SB_ROOT GC MAX_HEAP RES OUT GRADLE_JUNIT_CACHE CPCACHE_DIR crash_pattern T0

total=$(($(wc -l < "$ALLTESTS") - 1))
echo "[$GC] starting $total classes, parallel=$PARALLEL, base timeout=${BASE_TO}s"

running=0
idx=0
while IFS=$'\t' read -r module cls; do
  [ -z "$cls" ] && continue
  idx=$((idx + 1))
  run_one "$idx" "$module" "$cls" "$(effective_timeout "$module" "$cls")" &
  running=$((running + 1))
  if [ "$running" -ge "$PARALLEL" ]; then
    wait -n
    running=$((running - 1))
  fi
done < <(tail -n +2 "$ALLTESTS")
wait

t1=$(date +%s)
echo "=== [$GC] done in $((t1 - T0))s ==="
awk -F'\t' 'NR>1 {c[$4]++} END {for (k in c) printf "%s=%d\n", k, c[k]}' "$RES"

# A PASS whose wall lands within 15% of its own budget is a coin toss on the
# next run, and a HANG at a budget nothing validated is not evidence of a hang.
# Both are the reader's business, so print them here rather than leaving them
# to be noticed after the next sweep disagrees with this one.
awk -F'\t' 'NR>1 && $4=="PASS" && $6/1000 > 0.85*$7 {
  printf "NEAR-CAP PASS  %s/%s  %.0fs of %ss\n", $2, $3, $6/1000, $7 }' "$RES"
awk -F'\t' 'NR>1 && $4=="HANG" {
  printf "HANG  %s/%s  budget %ss%s\n", $2, $3, $7,
    ($7 <= 300 ? "  (base default -- no validated budget for this class)" : "") }' "$RES"
