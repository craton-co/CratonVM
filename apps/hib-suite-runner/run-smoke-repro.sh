#!/usr/bin/env bash
# =============================================================================
# run-smoke-repro.sh — N-run repro harness for
# `org.hibernate.orm.test.sql.exec.SmokeTests#testQueryConcurrency`.
#
# Docs: docs/internal/fixed-suite-bugs/hibernate/smoketests-stale-pointer-nosuchmethod-crash-20260804-RETIRED.md
#       docs/known-issues/hibernate/smoketests-concurrent-println-timeout-20260723.md (the 120 s timeout)
#
# The class is the only Hibernate fixture that parks the JUnit main thread in
# `ExecutorService.invokeAll` for 50x400 tasks while five workers churn the
# young generation, which is what makes it the second repro family for the
# blocked-thread root-maintenance path. One run is not evidence: the crash is
# timing-sensitive, so this script runs N times and reports a hit rate for each
# of the three signatures separately (stale receiver, reclaimed-slot audit,
# fatal NoSuchMethodError).
#
# Usage:
#   run-smoke-repro.sh -n 5 -b <cratonvm.exe> [-o <outdir>] [-x 1500m] [--nojit]
#                      [-e KEY=VALUE]...
# =============================================================================
set -uo pipefail
export MSYS2_ARG_CONV_EXCL='*'
export MSYS_NO_PATHCONV=1

HERE="C:/craton/CratonVM/apps/hib-suite-runner"
COMMON="$HERE/common.args"
CLASS="org.hibernate.orm.test.sql.exec.SmokeTests"

N=5
BIN="${CV_BIN:-}"
OUT=""
XMX="1500m"
TIMEOUT="${TIMEOUT:-900}"
EXTRA=()
ENVS=()

detect_jdk() {
  local c
  for c in "C:/Program Files/Eclipse Adoptium"/jdk-25* \
           "C:/Program Files/Java"/jdk-25* \
           "C:/Program Files/Eclipse Adoptium"/jdk-2* \
           "C:/Program Files/Java"/jdk-2*; do
    [ -x "$c/bin/java.exe" ] && { printf '%s' "$c"; return 0; }
  done
  return 1
}
JDK="${JDK:-$(detect_jdk)}"

while [ $# -gt 0 ]; do
  case "$1" in
    -n) N="$2"; shift 2;;
    -b|--bin) BIN="$2"; shift 2;;
    -o|--out) OUT="$2"; shift 2;;
    -x|--xmx) XMX="$2"; shift 2;;
    -t|--timeout) TIMEOUT="$2"; shift 2;;
    -e|--env) ENVS+=("$2"); shift 2;;
    --nojit) EXTRA+=(--nojit); shift;;
    --) shift; EXTRA+=("$@"); break;;
    *) EXTRA+=("$1"); shift;;
  esac
done

[ -n "$BIN" ] || { echo "ERROR: -b <cratonvm.exe> required" >&2; exit 2; }
[ -x "$BIN" ] || { echo "ERROR: not executable: $BIN" >&2; exit 2; }
[ -n "$JDK" ] || { echo "ERROR: no JDK found" >&2; exit 2; }
[ -n "$OUT" ] || OUT="$HERE/runs/smoke-repro-$(date +%Y%m%d-%H%M%S)"
mkdir -p "$OUT"

# CWD trap (see reference_hib_suite_runner_class_overrides_and_cwd_trap): the
# fixture's `GradleParallelTestingResolver.getWorkerID` reads a worker-id file
# relative to the CWD the forked VM inherits. From anywhere but the fixture
# directory every run dies in ~1 s with `FileNotFoundException` ->
# `ExceptionInInitializerError` in `JdbcConnectionContext.<clinit>`, which the
# harness would otherwise score as a CRASH hit. Pin it, exactly as run-hib.sh
# does, so a repro launched from a worktree measures the VM and not the CWD.
cd "$HERE" || { echo "ERROR: cannot cd to fixture dir $HERE" >&2; exit 2; }

echo "bin=$BIN"
echo "jdk=$JDK  xmx=$XMX  runs=$N  timeout=${TIMEOUT}s  extra=${EXTRA[*]:-none}  env=${ENVS[*]:-none}"
echo "out=$OUT"

hits_stale=0; hits_audit=0; hits_nsme=0; hits_crash=0; hits_pass=0
for ((i = 1; i <= N; i++)); do
  LOG="$OUT/run-$i.log"
  t0=$(date +%s)
  (
    for kv in "${ENVS[@]:-}"; do [ -n "$kv" ] && export "${kv?}"; done
    export CRATONVM_THREADS=-default-watchdog
    timeout "$TIMEOUT" "$BIN" --java-home "$JDK" --Xmx "$XMX" "${EXTRA[@]}" \
      @"$COMMON" -Dcraton.batch=1 CratonRunner "$CLASS"
  ) >"$LOG" 2>&1
  rc=$?
  t1=$(date +%s)

  s=$(grep -c "Stale pointer detected" "$LOG")
  a=$(grep -c "points into RECLAIMED memory" "$LOG")
  m=$(grep -c "NoSuchMethodError" "$LOG")
  r=$(grep -m1 "^@@RESULT " "$LOG")

  [ "$s" -gt 0 ] && hits_stale=$((hits_stale + 1))
  [ "$a" -gt 0 ] && hits_audit=$((hits_audit + 1))
  [ "$m" -gt 0 ] && hits_nsme=$((hits_nsme + 1))
  if [ -z "$r" ]; then hits_crash=$((hits_crash + 1)); else
    case "$r" in *"failed=0 aborted=0"*) hits_pass=$((hits_pass + 1));; esac
  fi

  printf 'run %-3s rc=%-4s %4ss  stale=%-5s reclaimed-slot=%-5s NSME=%-5s  %s\n' \
    "$i" "$rc" "$((t1 - t0))" "$s" "$a" "$m" "${r:-NO-@@RESULT (process died)}"
done

echo "---"
echo "runs=$N  stale_pointer=$hits_stale  reclaimed_slot_audit=$hits_audit  NSME=$hits_nsme  no_result=$hits_crash  clean_pass=$hits_pass"
