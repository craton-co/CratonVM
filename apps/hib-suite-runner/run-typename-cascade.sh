#!/usr/bin/env bash
# =============================================================================
# run-typename-cascade.sh — drive the SessionFactory rebuild cascade that the
# `java/lang/Integer.getTypeName()` wrong-receiver warning rides on.
#
# See docs/known-issues/hibernate/gettypename-wrong-receiver-in-sessionfactory-
# rebuild-cascade-20260801.md. The field witnesses did 21 SessionFactory
# bootstraps and emitted 32 warnings; every hunt attempt so far topped out at 5
# bootstraps, so the defect was never observed with a tracer on.
#
# The cascade's mechanism is not load — it is `SessionFactoryExtension`:
#
#   handleTestExecutionException(..) -> scope.releaseSessionFactory()
#
# so EVERY test that throws makes the next test rebuild the factory. In the
# field runs a 120s JUnit timeout under load threw once and the remaining tests
# each paid a ~26s rebuild until the harness's wall cap killed the run.
#
# Driving that on purpose is just a matter of shrinking the JUnit per-test
# timeout: at a few seconds every test method times out, so every test rebuilds,
# and one run reaches ~100 bootstraps instead of 3. `timeout.default` applies to
# @Test methods only — @BeforeAll (where the factory is actually built) is
# governed by `timeout.lifecycle.method.default`, which we leave alone so the
# bootstrap itself is never interrupted.
#
# usage: run-typename-cascade.sh [-b BIN] [-t JUNIT_TIMEOUT] [-n RUNS]
#                               [-w WALL] [-c CLASS] [-o OUTDIR] [-- vmflags..]
# =============================================================================
set -uo pipefail
export MSYS2_ARG_CONV_EXCL='*'
export MSYS_NO_PATHCONV=1

HERE="C:/craton/CratonVM/apps/hib-suite-runner"
COMMON="$HERE/common.args"

BIN="${CV_BIN:-C:/craton/CratonVM-typename-20260801/target/release/cratonvm.exe}"
JUNIT_TO="3s"
RUNS=4
WALL=1800
CLASS="org.hibernate.orm.test.hql.ASTParserLoadingTest"
OUTDIR=""
XMX="4g"
REPEAT=1
EXTRA=()

while [ $# -gt 0 ]; do
  case "$1" in
    -b) BIN="$2"; shift 2;;
    -t) JUNIT_TO="$2"; shift 2;;
    -n) RUNS="$2"; shift 2;;
    -w) WALL="$2"; shift 2;;
    -c) CLASS="$2"; shift 2;;
    -o) OUTDIR="$2"; shift 2;;
    -x) XMX="$2"; shift 2;;
    -r) REPEAT="$2"; shift 2;;
    --) shift; EXTRA=("$@"); break;;
    *) echo "bad arg: $1" >&2; exit 2;;
  esac
done

[ -f "$BIN" ] || { echo "ERROR: binary not found: $BIN" >&2; exit 1; }

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
JDK="$(detect_jdk)" || { echo "ERROR: no JDK found" >&2; exit 1; }

TS="$(date +%Y%m%d-%H%M%S)"
[ -n "$OUTDIR" ] || OUTDIR="$HERE/analysis/typename-cascade-$TS"
mkdir -p "$OUTDIR"
# The VM is a native Windows binary: an MSYS-form `/c/...` path handed to
# `@argfile` is not opened, the `@` arg falls through to the main-class slot and
# the run dies with "Invalid class name '@/c/...'". Normalise before use.
OUTDIR="$(cd "$OUTDIR" && { pwd -W 2>/dev/null || pwd; })"

# The argfile is common.args with the JUnit per-test timeout rewritten. Keep
# everything else byte-identical so a hit here is a hit under the real harness.
ARGS="$OUTDIR/cascade.args"
sed "s#junit.jupiter.execution.timeout.default=.*#junit.jupiter.execution.timeout.default=$JUNIT_TO#" \
    "$COMMON" > "$ARGS"
grep -q "timeout.default=$JUNIT_TO" "$ARGS" || {
  echo "ERROR: junit timeout not rewritten in $ARGS" >&2; exit 1; }

echo "bin=$BIN"
echo "class=$CLASS x$REPEAT junit_timeout=$JUNIT_TO runs=$RUNS wall=${WALL}s xmx=$XMX"
echo "out=$OUTDIR"

# `CratonRunner` loops over its argv, running each named class in the SAME JVM,
# so repeating the class re-enters `@SessionFactory` setup with a JIT that is
# already warm from a full pass of real query execution. That is the field
# witness's shape: its cascade began on the SIXTH bootstrap, after the whole
# test body had run — not on a cold one (see the known-issue doc).
CLASS_ARGV=()
for ((k=0; k<REPEAT; k++)); do CLASS_ARGV+=("$CLASS"); done

run_one() {
  local i="$1" log="$OUTDIR/run-$1.log"
  CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 \
  CRATONVM_DBG_CCE_BT=1 \
  timeout "$WALL" "$BIN" --java-home "$JDK" --Xmx "$XMX" "${EXTRA[@]}" @"$ARGS" \
      -Dcraton.batch=1 CratonRunner "${CLASS_ARGV[@]}" > "$log" 2>&1
  echo "$?" > "$OUTDIR/run-$i.rc"
}

pids=()
for ((i=0; i<RUNS; i++)); do run_one "$i" & pids+=($!); done
for p in "${pids[@]}"; do wait "$p"; done

# --- report ------------------------------------------------------------------
# Bootstraps are counted off the JavaTypeRegistry's own priming, which runs once
# per TypeConfiguration; `Database info` is the stable per-build banner the field
# logs were counted with.
# `grep -c` exits 1 on no match, so every count needs its own subshell guard —
# an `|| echo 0` on the same command substitution appends a SECOND line and
# silently shifts every remaining column.
count() { local n; n=$(grep -c "$2" "$1" 2>/dev/null); printf '%s' "${n:-0}"; }

printf '%-5s %-5s %-7s %-9s %-6s %s\n' run rc boots warns dumps result
for ((i=0; i<RUNS; i++)); do
  log="$OUTDIR/run-$i.log"
  rc="$(cat "$OUTDIR/run-$i.rc" 2>/dev/null)"
  res="$(grep -m1 '^@@RESULT ' "$log" 2>/dev/null | cut -c1-70)"
  printf '%-5s %-5s %-7s %-9s %-6s %s\n' "$i" "${rc:-?}" \
      "$(count "$log" 'Database info')" \
      "$(count "$log" 'getTypeName')" \
      "$(count "$log" 'CRATONVM_DBG_CCE_BT: site=')" \
      "${res:--}"
done

hitlogs=()
for ((i=0; i<RUNS; i++)); do
  if grep -q 'getTypeName' "$OUTDIR/run-$i.log" 2>/dev/null; then hitlogs+=("$OUTDIR/run-$i.log"); fi
done
echo
echo "logs with a getTypeName warning: ${#hitlogs[@]} / $RUNS"
if [ "${#hitlogs[@]}" -gt 0 ]; then
  echo "--- first occurrence, with the CCE_BT dump that follows it ---"
  grep -A 60 -m1 'getTypeName' "${hitlogs[0]}"
fi
