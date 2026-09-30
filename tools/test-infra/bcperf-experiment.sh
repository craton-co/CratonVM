#!/usr/bin/env bash
# BC-suite JIT-ban experiment: default vs CRATONVM_JIT_ALLOW_PACKAGES=org/bouncycastle/
set +e
export MSYS2_ARG_CONV_EXCL='*'
export MSYS_NO_PATHCONV=1

ROOT="${ROOT:-C:/craton/CratonVM}"
CV="${CV:-$ROOT/target/release/cratonvm.exe}"
JDK="${JDK:-C:/Program Files/Java/jdk-25}"
BC="$ROOT/apps/_test-suites/bc-java"
BCCP="$BC/core/build/classes/java/main;$BC/core/build/classes/java/test;$BC/core/build/resources/main;$BC/core/build/resources/test"
TS=$(date +%Y%m%d-%H%M%S)
OUT="${OUT:-$ROOT/test-infra/suite-results/bcperf-$TS}"
mkdir -p "$OUT"
TIMEOUT="${TIMEOUT:-420}"

# Kill only stray cratonvm.exe launched from THIS tree (avoid the taskkill-by-image-name footgun)
kill_stray() {
  local root_win
  root_win=$(printf '%s' "$ROOT" | sed 's|/|\\\\|g')
  powershell.exe -NoProfile -Command \
    "Get-CimInstance Win32_Process -Filter \"Name='cratonvm.exe'\" |
       Where-Object { \$_.ExecutablePath -and \$_.ExecutablePath -like '${root_win}*' } |
       ForEach-Object { Stop-Process -Id \$_.ProcessId -Force -ErrorAction SilentlyContinue }" \
    >/dev/null 2>&1 || true
}

run() {
  local label="$1" main="$2"
  kill_stray
  local t0=$(date +%s%N)
  timeout "$TIMEOUT" "$CV" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx 1g \
    -cp "$BCCP" "$main" </dev/null >"$OUT/$label.log" 2>&1
  local rc=$?
  local t1=$(date +%s%N)
  local wall=$(awk -v ms=$(((t1-t0)/1000000)) 'BEGIN{printf "%.1f", ms/1000}')
  local crash=""
  grep -qaiE "SEGV|panic|abort|fatal error|stack overflow|implausible" "$OUT/$label.log" && crash=" CRASH-SIG"
  echo "RESULT $label rc=$rc wall=${wall}s$crash last='$(tail -1 "$OUT/$label.log" | head -c 100)'"
}

echo "=== output dir: $OUT ==="
echo "--- baseline (ban in effect) ---"
run asn1-default org.bouncycastle.asn1.test.RegressionTest
run prng-default org.bouncycastle.crypto.prng.test.RegressionTest

echo "--- override: CRATONVM_JIT_ALLOW_PACKAGES=org/bouncycastle/ ---"
export CRATONVM_JIT_ALLOW_PACKAGES=org/bouncycastle/
export CRATONVM_DISABLE_DEFAULT_WATCHDOG=1
run asn1-allow org.bouncycastle.asn1.test.RegressionTest
run prng-allow org.bouncycastle.crypto.prng.test.RegressionTest

echo "--- EC AllTests canary under override ---"
run mathec-allow org.bouncycastle.math.ec.test.AllTests
echo "=== done ==="
