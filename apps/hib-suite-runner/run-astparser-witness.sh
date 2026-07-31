#!/usr/bin/env bash
# =============================================================================
# run-astparser-witness.sh — repeat the ANTLR moving-young witness class
#
# Runs org.hibernate.orm.test.hql.ASTParserLoadingTest N times against one
# cratonvm binary and prints one `run=<i> ok=<n> failed=<n>` line per run plus
# a final tally. This is the acceptance gate for
# docs/.../antlr-native-roots-moving-young-hql-misparse-20260730.md: the class
# mis-parses valid HQL nondeterministically when a native ANTLR intrinsic loses
# an object root across a moving young collection, so a single green run proves
# nothing — only a run count does.
#
# usage: run-astparser-witness.sh --bin <cratonvm.exe> [--runs N] [--jit on|off]
#                                [--class FQCN] [--tag NAME]
# =============================================================================
set -uo pipefail
export MSYS2_ARG_CONV_EXCL='*'
export MSYS_NO_PATHCONV=1

HERE="C:/craton/CratonVM/apps/hib-suite-runner"
COMMON="$HERE/common.args"
RUNNER_CLASS="CratonRunner"
JDK="${JDK:-C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot}"
CV_XMX="${CV_XMX:-1500m}"
TIMEOUT="${TIMEOUT:-600}"

BIN=""
RUNS=10
JITMODE="off"
CLS="org.hibernate.orm.test.hql.ASTParserLoadingTest"
TAG="witness"

while [ $# -gt 0 ]; do
  case "$1" in
    --bin)   BIN="$2"; shift 2;;
    --runs)  RUNS="$2"; shift 2;;
    --jit)   JITMODE="$2"; shift 2;;
    --class) CLS="$2"; shift 2;;
    --tag)   TAG="$2"; shift 2;;
    *) echo "unknown option: $1" >&2; exit 2;;
  esac
done

[ -f "$BIN" ] || { echo "ERROR: binary not found: $BIN (pass --bin)" >&2; exit 1; }

VMFLAGS=(--java-home "$JDK" --Xmx "$CV_XMX")
[ "$JITMODE" = off ] && VMFLAGS+=(--nojit)
VMFLAGS+=(@"$COMMON")

OUT="$HERE/runs/witness-$TAG-$(date +%Y%m%d-%H%M%S)"
mkdir -p "$OUT"
echo "=== $TAG :: $CLS x$RUNS | jit=$JITMODE | bin=$BIN ==="

total_fail_runs=0
total_failed=0
for ((i=0; i<RUNS; i++)); do
  log="$OUT/run-$i.log"
  CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 timeout "$TIMEOUT" "$BIN" "${VMFLAGS[@]}" \
      -Dcraton.batch=1 "$RUNNER_CLASS" "$CLS" >"$log" 2>&1
  rc=$?
  rline=$(grep -m1 "^@@RESULT " "$log")
  if [ -z "$rline" ]; then
    echo "run=$i STATUS=$([ $rc -eq 124 ] && echo HANG || echo CRASH) rc=$rc"
    total_fail_runs=$((total_fail_runs+1))
    continue
  fi
  ok=$(printf '%s' "$rline"    | grep -o 'ok=[0-9]*'      | cut -d= -f2)
  failed=$(printf '%s' "$rline"| grep -o 'failed=[0-9]*'  | cut -d= -f2)
  found=$(printf '%s' "$rline" | grep -o 'found=[0-9]*'   | cut -d= -f2)
  ms=$(printf '%s' "$rline"    | grep -o 'ms=[0-9]*'      | cut -d= -f2)
  echo "run=$i found=${found:-0} ok=${ok:-0} failed=${failed:-0} ms=${ms:-0}"
  total_failed=$((total_failed + ${failed:-0}))
  [ "${failed:-0}" -gt 0 ] && total_fail_runs=$((total_fail_runs+1))
done

echo "=== $TAG SUMMARY: runs=$RUNS runs_with_failures=$total_fail_runs total_failed_tests=$total_failed  logs=$OUT ==="
