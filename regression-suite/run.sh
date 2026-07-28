#!/usr/bin/env bash
# CratonVM fast regression suite.
#
# Compiles regression-suite/src/*.java once, then runs each test class on
# CratonVM and (if available) HotSpot, comparing the deterministic output. A
# class PASSES when CratonVM exits 0, prints its "PASS <Class>" line, does not
# crash, AND its checksum/output lines match HotSpot's. Exits non-zero if any
# class fails — suitable for CI.
#
# Env overrides: CV=<cratonvm.exe>  JDK=<jdk home>  ONLY="RJitGc RCrypto"
set +e
export MSYS2_ARG_CONV_EXCL='*'; export MSYS_NO_PATHCONV=1

ROOT="$(git -C "$(dirname "${BASH_SOURCE[0]}")" rev-parse --show-toplevel 2>/dev/null || echo C:/craton/CratonVM)"
HERE="$ROOT/regression-suite"
CV="${CV:-$ROOT/target/release/cratonvm.exe}"
JDK="${JDK:-${JAVA_HOME:-C:/Program Files/Java/jdk-25}}"
# The suite is usually run from Git Bash on Windows, but the Linux build host
# is where the JIT fixes are validated first — fall back to the extension-less
# names (and the extension-less CratonVM binary) when the .exe form is absent.
JAVAC="$JDK/bin/javac.exe"; HS="$JDK/bin/java.exe"
[ -x "$JAVAC" ] || { JAVAC="$JDK/bin/javac"; HS="$JDK/bin/java"; }
[ -x "$CV" ] || { case "$CV" in *.exe) [ -x "${CV%.exe}" ] && CV="${CV%.exe}" ;; esac; }
BUILD="$HERE/build"
TIMEOUT="${TIMEOUT:-120}"

# Default test set — the reliably-green, fast baseline. Add classes here as the
# suite grows. RConcurrent is intentionally NOT in the default set: it exercises
# heavy multi-threaded execution, which intermittently trips a documented
# CratonVM gap (cross-thread JIT-frame root scanning at a STW GC pause — see
# README "Known gaps"), so it flakes. Run it explicitly once that gap is closed:
#   ONLY="RConcurrent" bash regression-suite/run.sh
CLASSES="${ONLY:-RCollections RStrings RNumbers RSerial RCrypto RExceptions RReflect ROptionalClassForName RPrivateLambdaOwner RLambdaDefaultOverload RJitGc RJitStringLayout RJitArrayTypecheck RExecutorShutdown RChannelInterrupt RSocketChannelInterrupt RAtomicArray}"

[ -x "$CV" ] || { echo "ERROR: CratonVM binary not found: $CV (build with build-cpu.bat)"; exit 3; }
[ -x "$JAVAC" ] || { echo "ERROR: javac not found: $JAVAC (set JDK=...)"; exit 3; }

echo "== compiling regression-suite =="
rm -rf "$BUILD"; mkdir -p "$BUILD"
"$JAVAC" -d "$BUILD" "$HERE"/src/*.java || { echo "ERROR: javac failed"; exit 3; }

# Extract only the deterministic test lines (PASS/CK), stripping CratonVM's
# timestamped WARN/tracing noise and ANSI colour, so the cross-VM diff is clean.
extract() { sed 's/\x1b\[[0-9;]*m//g' | grep -aE '^(PASS|CK) ' ; }

pass=0; fail=0; failed=""
for c in $CLASSES; do
  cvout=$(CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 timeout "$TIMEOUT" "$CV" --java-home "$JDK" -cp "$BUILD" "$c" 2>&1)
  cvrc=$?
  cvkey=$(printf '%s\n' "$cvout" | extract)
  # A failed assertion throws AssertionError → non-zero exit (handled by the rc
  # check), so we do NOT broad-grep for "Exception"/"Error" — tests intentionally
  # throw-and-catch, and CratonVM traces those, which would false-fail. We only
  # flag hard VM crashes that may not set a non-zero rc.
  state=PASS; why=""
  if [ "$cvrc" -ne 0 ]; then
    state=FAIL; why="cratonvm rc=$cvrc"
    sig=$(printf '%s\n' "$cvout" | grep -aiE 'AssertionError|NoSuchMethod|linkage error|panic|SEGV|fatal' | grep -avE '^\s*at ' | tail -1 | sed 's/\x1b\[[0-9;]*m//g' | head -c 90)
    [ -n "$sig" ] && why="rc=$cvrc: $sig"
  elif printf '%s' "$cvout" | grep -qaiE 'SIGSEGV|rust panic|fatal runtime error|stack overflow'; then
    state=FAIL; why="VM crash"
  elif ! printf '%s\n' "$cvkey" | grep -qaE "^PASS $c"; then
    state=FAIL; why="no PASS line"
  fi
  # Cross-VM diff against HotSpot (when present).
  if [ "$state" = PASS ] && [ -x "$HS" ]; then
    hskey=$(timeout "$TIMEOUT" "$HS" -cp "$BUILD" "$c" 2>&1 | extract)
    if [ "$cvkey" != "$hskey" ]; then
      state=FAIL; why="output differs from HotSpot"
      printf '    --- HotSpot ---\n%s\n    --- CratonVM ---\n%s\n' "$hskey" "$cvkey" | sed 's/^/    /'
    fi
  fi
  if [ "$state" = PASS ]; then pass=$((pass+1)); printf "  %-14s PASS\n" "$c"
  else fail=$((fail+1)); failed="$failed $c"; printf "  %-14s FAIL  %s\n" "$c" "$why"; fi
done

echo "---------------------------------------------"
echo "REGRESSION SUITE: $pass passed, $fail failed${failed:+ ( failed:$failed )}"
[ "$fail" -eq 0 ]
