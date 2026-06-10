#!/usr/bin/env bash
# Fresh-build CPU rerun: micro-benchmarks + apps suites (CratonVM-CPU vs HotSpot).
set +e
export MSYS2_ARG_CONV_EXCL='*'
export MSYS_NO_PATHCONV=1
ROOT="${ROOT:-C:/craton/CratonVM}"
JDK="${JDK:-C:/Program Files/Java/jdk-25}"
CV="$ROOT/target-fresh-cpu/release/cratonvm.exe"
LOG="$ROOT/test-infra/suite-results/cpu-rerun-$(date +%Y%m%d-%H%M%S).log"
mkdir -p "$ROOT/test-infra/suite-results" "$ROOT/target/release"
exec > >(tee -a "$LOG") 2>&1

echo "=== CPU rerun — build + bench + apps ==="
echo "Log: $LOG"
taskkill //F //IM cratonvm.exe 2>/dev/null || true

echo "--- compile bench + harness ---"
"$JDK/bin/javac.exe" -d "$ROOT/bench" "$ROOT/bench/BenchSuite.java" 2>&1
WFCP=$(cat "$ROOT/apps/wildfly/health/cratonvm-health-cp.txt" 2>/dev/null)
"$JDK/bin/javac.exe" -cp "$WFCP" -d "$ROOT/apps/_test-harness" "$ROOT/apps/_test-harness/RunDirTests.java" 2>&1 || true
JUNIT="$ROOT/.bench-cache/junit-platform-console-standalone-1.10.2.jar"
CM="$ROOT/apps/_test-suites/commons-math"
M2="$HOME/.m2/repository"
ABS="$JUNIT;$CM/commons-math-transform/target/classes;$CM/commons-math-transform/target/test-classes;$CM/commons-math-core/target/classes"
[ -f "$JUNIT" ] && "$JDK/bin/javac.exe" -cp "$ABS" -d "$ROOT/bench" "$ROOT/bench/JUnitProbe.java" 2>&1 || true

if [ ! -x "$CV" ]; then
  echo "ERROR: $CV missing — run scripts/build-cpu-isolated.bat first"
  exit 3
fi
cp -f "$CV" "$ROOT/target/release/cratonvm.exe"
ls -la "$ROOT/target/release/cratonvm.exe"

echo "--- CPU micro-benchmarks (CratonVM-CPU vs HotSpot) ---"
export CV="$ROOT/target/release/cratonvm.exe"
export SECTIONS="bench render"
export VARIANTS="cratonvm-cpu hotspot"
export HEAP=8g TIMEOUT=360
bash "$ROOT/test-infra/run-vm-comparison.sh"

echo "--- apps test suites ---"
bash "$ROOT/test-infra/run-all-apps-suites.sh"

echo "=== Done ==="
