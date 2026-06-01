#!/usr/bin/env bash
# Commons Math (Apache) full Surefire reactor across the 4 variants, with
# wall-time + pass/total. Appends to a TSV. Run AFTER the bc/dacapo 4-way so
# timings stay serial (no CPU contention).
set +e
ROOT="C:/craton/CratonVM"
CM_DIR="$ROOT/apps/_test-suites/commons-math"
JDK="C:/Program Files/Java/jdk-25"
MVN="/c/tools/apache-maven-3.9.15/bin/mvn"
SHIM_CPU="$(cygpath -w "$ROOT/test-infra/cratonvm-java-shim.bat")"
SHIM_GPU="$(cygpath -w "$ROOT/test-infra/cratonvm-gpu-driver-java-shim.bat")"
SHIM_TORN="$(cygpath -w "$ROOT/test-infra/tornadovm-java-shim.bat")"
OUT="${1:-$ROOT/test-infra/suite-results/commons-math-4way-$(date +%Y%m%d-%H%M%S).tsv}"
TIMEOUT="${TIMEOUT:-1200}"
VARIANTS="${VARIANTS:-cratonvm-cpu cratonvm-gpu hotspot tornadovm}"
[ -f "$OUT" ] || printf "suite\ttask\tvariant\trc\tpass\twall_s\tnote\n" > "$OUT"

run_cm() { # variant djvm-shim-or-empty
  local variant="$1" shim="$2"
  local log; log=$(mktemp)
  local args=(-o -B -ntp -Dmaven.test.failure.ignore=true -Drat.skip=true -Dcheckstyle.skip=true -Denforcer.skip=true -Dspotbugs.skip=true -Danimal.sniffer.skip=true test)
  [ -n "$shim" ] && args=(-o -B -ntp "-Djvm=$shim" -Dmaven.test.failure.ignore=true -Drat.skip=true -Dcheckstyle.skip=true -Denforcer.skip=true -Dspotbugs.skip=true -Danimal.sniffer.skip=true test)
  local t0 t1 rc
  t0=$(date +%s)
  ( cd "$CM_DIR" && JAVA_HOME="$JDK" timeout "$TIMEOUT" "$MVN" "${args[@]}" ) > "$log" 2>&1
  rc=$?; t1=$(date +%s)
  # Surefire reactor summary: "Tests run: N, Failures: F, Errors: E, Skipped: S"
  local summary; summary=$(grep -E 'Tests run: [0-9]+, Failures' "$log" | tail -1)
  local total fails errs
  total=$(echo "$summary" | grep -oE 'Tests run: [0-9]+' | grep -oE '[0-9]+' | head -1)
  fails=$(echo "$summary" | grep -oE 'Failures: [0-9]+' | grep -oE '[0-9]+' | head -1)
  errs=$(echo "$summary"  | grep -oE 'Errors: [0-9]+'   | grep -oE '[0-9]+' | head -1)
  local pass=FAIL
  if [ "$rc" -eq 124 ]; then pass=TIMEOUT
  elif [ -n "$total" ] && [ "${fails:-1}" = "0" ] && [ "${errs:-1}" = "0" ]; then pass=PASS
  elif [ -n "$total" ]; then pass="${fails:-?}F/${errs:-?}E"; fi
  local passcount=$(( ${total:-0} - ${fails:-0} - ${errs:-0} ))
  printf "%-13s %-16s %-13s rc=%-3s %-10s %5ss  %s\n" "commons-math" "reactor" "$variant" "$rc" "$pass" "$((t1-t0))" "${summary:-<no summary>}"
  printf "commons-math\treactor(%s/%s)\t%s\t%s\t%s\t%s\t%s\n" "$passcount" "${total:-?}" "$variant" "$rc" "$pass" "$((t1-t0))" "${summary:-none}" >> "$OUT"
  cp "$log" "$ROOT/test-infra/suite-results/cm-$variant-last.log"
  rm -f "$log"
}

for v in $VARIANTS; do
  echo "########## commons-math: $v ##########"
  case "$v" in
    cratonvm-cpu) run_cm "$v" "$SHIM_CPU";;
    cratonvm-gpu) run_cm "$v" "$SHIM_GPU";;
    hotspot)      run_cm "$v" "";;            # default JVM = JAVA_HOME (hotspot)
    tornadovm)    run_cm "$v" "$SHIM_TORN";;
  esac
done
echo "Results: $OUT"
