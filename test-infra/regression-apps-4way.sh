#!/usr/bin/env bash
# Per-app JUnit suite x N-way runner. Runs each concrete test class in its own
# VM (one process per class so a SEGV doesn't lose the batch), across the
# requested variants, recording pass/fail + wall_s into a TSV.
#
#   Variants: cratonvm-cpu | cratonvm-gpu | hotspot | tornadovm
#
# Usage:
#   regression-apps-4way.sh <app> <cp-string> <variants> <class1> [class2 ...]
#
# <app>        label for the TSV (keycloak|wildfly|...)
# <cp-string>  full classpath (Windows ';'-separated)
# <variants>   space-separated variant list (quote it)
set +e

ROOT="C:/craton/CratonVM"
CV_CPU="${CV_CPU:-$ROOT/target/release/cratonvm.exe}"
CV_GPU="${CV_GPU:-$ROOT/target-gpu/release/cratonvm.exe}"
JDK="C:/Program Files/Java/jdk-25"
HOTSPOT="$JDK/bin/java.exe"
TORNADO="C:/craton/tornadovm/jdk-25.0.3/bin/java.exe"
TORNADO_ARGF="@C:/craton/tornadovm/tornadovm-4.0.1-jdk25-ptx/tornado-argfile"
TIMEOUT="${TIMEOUT:-150}"

APP="$1"; CP="$2"; VARIANTS="$3"; shift 3
CLASSES=("$@")

TS=$(date +%Y%m%d-%H%M%S)
OUT="${OUT:-$ROOT/test-infra/suite-results/${APP}-Nway-$TS.tsv}"
mkdir -p "$(dirname "$OUT")"
[ -f "$OUT" ] || printf "app\tclass\tvariant\trc\tpass\twall_s\tnote\n" > "$OUT"

jvm_prefix() {
  case "$1" in
    cratonvm-cpu) JPRE=("$CV_CPU" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx 1g);;
    cratonvm-gpu) JPRE=("$CV_GPU" --gpu --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx 1g);;
    hotspot)      JPRE=("$HOTSPOT" -Xmx1g);;
    tornadovm)    JPRE=("$TORNADO" "$TORNADO_ARGF" -Xmx1g);;
  esac
}

for v in $VARIANTS; do
  echo "########## $APP: variant $v ##########"
  pass=0; fail=0; crash=0; tpass=0; tfail=0; tcrash=0
  for cls in "${CLASSES[@]}"; do
    jvm_prefix "$v"
    log=$(mktemp)
    t0=$(date +%s)
    timeout "$TIMEOUT" "${JPRE[@]}" -cp "$CP" org.junit.runner.JUnitCore "$cls" < /dev/null > "$log" 2>&1
    rc=$?; t1=$(date +%s)
    res=CRASH
    if [ "$rc" -eq 0 ] && grep -q "^OK" "$log"; then res=PASS
    elif grep -qE "^Tests run:.*Failures" "$log"; then res=FAIL; fi
    [ "$rc" -eq 124 ] && res=TIMEOUT
    note=$(grep -aoE "OK \([0-9]+ test[s]?\)|Tests run: [0-9]+,[^)]*|InstantiationException" "$log" | head -1)
    case "$res" in PASS) pass=$((pass+1));; FAIL) fail=$((fail+1));; *) crash=$((crash+1));; esac
    printf "  %-5s rc=%-3s %4ss  %-62s %s\n" "$res" "$rc" "$((t1-t0))" "$cls" "$note"
    printf "%s\t%s\t%s\t%s\t%s\t%s\t%s\n" "$APP" "$cls" "$v" "$rc" "$res" "$((t1-t0))" "$note" >> "$OUT"
    rm -f "$log"
  done
  printf "  ---- %s/%s: PASS=%d FAIL=%d CRASH=%d ----\n" "$APP" "$v" "$pass" "$fail" "$crash"
done
echo "Results: $OUT"
