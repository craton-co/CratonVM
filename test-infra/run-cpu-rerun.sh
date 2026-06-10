#!/usr/bin/env bash
# Fresh-build CPU rerun: micro-benchmarks + apps suites (CratonVM-CPU vs HotSpot).
set +e
export MSYS2_ARG_CONV_EXCL='*'
export MSYS_NO_PATHCONV=1
ROOT="${ROOT:-$(git -C "$(dirname "${BASH_SOURCE[0]}")" rev-parse --show-toplevel 2>/dev/null || echo C:/craton/CratonVM)}"
JDK="${JDK:-${JAVA_HOME:-C:/Program Files/Java/jdk-25}}"
CV="$ROOT/target-fresh-cpu/release/cratonvm.exe"
LOG="$ROOT/test-infra/suite-results/cpu-rerun-$(date +%Y%m%d-%H%M%S).log"
mkdir -p "$ROOT/test-infra/suite-results" "$ROOT/target/release"
exec > >(tee -a "$LOG") 2>&1

echo "=== CPU rerun — build + bench + apps ==="
echo "Log: $LOG"

# Kill ONLY leftover cratonvm.exe launched from THIS tree ($ROOT) so the copy
# below isn't blocked by a locked binary, without touching a parallel suite /
# another worktree / a dev session; never surface a no-match as a failure.
kill_stray_cratonvm() {
  local root_win
  root_win=$(printf '%s' "$ROOT" | sed 's|/|\\\\|g')
  powershell.exe -NoProfile -Command \
    "Get-CimInstance Win32_Process -Filter \"Name='cratonvm.exe'\" |
       Where-Object { \$_.ExecutablePath -and \$_.ExecutablePath -like '${root_win}*' } |
       ForEach-Object { Stop-Process -Id \$_.ProcessId -Force -ErrorAction SilentlyContinue }" \
    >/dev/null 2>&1 || true
}
kill_stray_cratonvm

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
