#!/usr/bin/env bash
#
# wcit-exchange-ab.sh — drive the WebClientIntegrationTests exchange probes.
#
# The class's 170 tests all do the same thing: fresh MockWebServer + WebClient +
# one request + close. `ExchangeProbe` is that shape without JUnit and
# reproduces the whole gap; `ExchangePhases` splits it; `ShadowProbe2` is the
# per-call arm set. See
# internal/performance/webclient-integration-tests-reactive-exchange-gap-RETIRED-20260823.md
#
# Usage:
#   CRATONVM_BIN=<bin> ./wcit-exchange-ab.sh run   <hs|cv> <MainClass> [args...]
#   CRATONVM_BIN=<bin> ./wcit-exchange-ab.sh marginal <connector> <n_low> <n_high>
#                      ./wcit-exchange-ab.sh ab     <binA> <binB> <rounds> <MainClass> [args...]
#
# `marginal` is the one that matters for sizing: a raw `perf stat` total is
# dominated by VM startup (HotSpot's C2/GC threads make its total look almost
# equal to CratonVM's), so it runs two loop counts and subtracts, reporting the
# startup-free wall and CPU cost of ONE exchange.
#
# ALWAYS interleave. On the shared Azure host consecutive same-config runs of
# ExchangeProbe ranged 23.4-31.4 ms/op inside one four-minute window.
set -u

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
: "${SPRING_MOD:=/data/cratonvm/apps/spring-framework/spring-webflux}"
: "${JDK:=/data/toolchain/jdk-25}"
: "${PROBE_OUT:=/tmp/wcit}"
mkdir -p "$PROBE_OUT"

# Classpath is `cratonvm-testcp.txt` VERBATIM (a dump of Gradle's own
# sourceSets.test.runtimeClasspath) with the probe classes ahead of it. Nothing
# else may be prepended — see apps/spring-suite-runner/one.sh for the two
# measured failures that rule causes.
CP="$PROBE_OUT:$(tr -d '\r' < "$SPRING_MOD/build/cratonvm-testcp.txt")"

# spring-framework buildSrc TestConventions gives every test JVM these.
JVM_ARGS=(
  --add-opens=java.base/java.lang=ALL-UNNAMED
  --add-opens=java.base/java.util=ALL-UNNAMED
  -Djava.awt.headless=true
  -Dio.netty.leakDetection.level=paranoid
  -Djunit.platform.discovery.issue.severity.critical=INFO
  --enable-native-access=ALL-UNNAMED
)

compile_probes() {
  for f in "$HERE"/ExchangeProbe.java "$HERE"/ExchangePhases.java \
           "$HERE"/ShadowProbe2.java "$HERE"/GetNanoProbe.java "$HERE"/InstantProbe2.java; do
    [ -f "$f" ] || continue
    "$JDK/bin/javac" -nowarn -cp "$CP" -d "$PROBE_OUT" "$f" 2>/dev/null
  done
}

run_one() {                     # run_one <hs|cv> <log> <Main> [args...]
  local kind="$1" log="$2"; shift 2
  cd "$SPRING_MOD" || return 1
  if [ "$kind" = hs ]; then
    timeout "${PTO:-900}" "$JDK/bin/java" "${JVM_ARGS[@]}" -Xshare:off \
      -cp "$CP" "$@" > "$log" 2>&1
  else
    local af; af="$(mktemp "$PROBE_OUT/af-XXXXXX.txt")"
    { echo "-cp"; echo "$CP"; } > "$af"
    timeout "${PTO:-900}" "${CRATONVM_BIN:?set CRATONVM_BIN}" --java-home "$JDK" \
      "${JVM_ARGS[@]}" ${CV_EXTRA:-} "@$af" "$@" > "$log" 2>&1
  fi
}

cmd="${1:-run}"; shift || true
compile_probes

case "$cmd" in
  run)
    kind="$1"; shift
    run_one "$kind" "$PROBE_OUT/run.log" "$@"
    grep -aE 'ms/op|PHASES-DONE' "$PROBE_OUT/run.log"
    ;;

  marginal)
    conn="${1:-jetty}"; lo="${2:-40}"; hi="${3:-200}"
    for arm in hs cv hs cv; do
      declare -A W C
      for n in "$lo" "$hi"; do
        t0=$(date +%s%N)
        perf stat -e task-clock -x, -o "$PROBE_OUT/mg-$arm-$n.stat" -- \
          bash "$0" run "$arm" ExchangeProbe "$conn" "$n" >/dev/null 2>&1
        t1=$(date +%s%N)
        W[$n]=$(( (t1-t0)/1000000 ))
        C[$n]=$(awk -F, '$3=="task-clock"{printf "%.0f", $1/1e6}' "$PROBE_OUT/mg-$arm-$n.stat")
      done
      d=$((hi-lo))
      printf 'MARGINAL %-3s wall/exchange=%.2fms  cpu/exchange=%.2fms\n' "$arm" \
        "$(awk -v a=$(( ${W[$hi]} - ${W[$lo]} )) -v b=$d 'BEGIN{print a/b}')" \
        "$(awk -v a=$(( ${C[$hi]} - ${C[$lo]} )) -v b=$d 'BEGIN{print a/b}')"
    done
    ;;

  ab)
    binA="$1"; binB="$2"; rounds="$3"; shift 3
    r=0
    while [ "$r" -lt "$rounds" ]; do
      # forward then reverse: position in the round cannot favour an arm
      for arm in A B B A; do
        if [ "$arm" = A ]; then b="$binA"; else b="$binB"; fi
        CRATONVM_BIN="$b" run_one cv "$PROBE_OUT/ab-$arm.log" "$@"
        echo "$arm $(grep -aE 'ms/op' "$PROBE_OUT/ab-$arm.log" | tail -20 | tr '\n' '|')"
      done
      r=$((r+1))
    done
    ;;

  *) echo "usage: $0 {run|marginal|ab} ..." >&2; exit 2 ;;
esac
