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
#                TIMEOUT=<seconds>
#
# JDK-only mode (docs/feature-designs/jdk-only-mode.md):
#   CRATONVM_ARGS="--jdk-only"   extra args handed to the CratonVM launcher
#                                (word-split; empty by default, in which case
#                                the invocation is byte-for-byte what it was)
#   SUITE=core|jdk-only|all      which class list to run; default core, which
#                                is exactly the historical set
#   RELEASES="17 21 25"          javac --release matrix; empty (the default)
#                                means one pass with no --release flag at all
#   JDK17=/path JDK21=... JDK25= optional per-release JDK homes; when unset the
#                                main $JDK is used with --release <n>
set +e
export MSYS2_ARG_CONV_EXCL='*'; export MSYS_NO_PATHCONV=1

ROOT="$(git -C "$(dirname "${BASH_SOURCE[0]}")" rev-parse --show-toplevel 2>/dev/null || echo C:/craton/CratonVM)"
HERE="$ROOT/regression-suite"
CV="${CV:-$ROOT/target/release/cratonvm.exe}"
JDK="${JDK:-${JAVA_HOME:-C:/Program Files/Java/jdk-25}}"
[ -x "$CV" ] || { case "$CV" in *.exe) [ -x "${CV%.exe}" ] && CV="${CV%.exe}" ;; esac; }
BUILD="$HERE/build"
MBUILD="$HERE/build-modules"
RESOURCES="$HERE/resources"
MODULES_SRC="$HERE/modules"
EXPECT="$HERE/expect"
TIMEOUT="${TIMEOUT:-120}"
SUITE="${SUITE:-core}"
RELEASES="${RELEASES:-}"
CRATONVM_ARGS="${CRATONVM_ARGS:-}"

# The named module the JDK-only corpus resolves for RJdkModule.
SVC_MODULE="cratonvm.jdkonly.svc"

# The suite is usually run from Git Bash on Windows, but the Linux build host
# is where the JIT fixes are validated first — fall back to the extension-less
# names (and the extension-less CratonVM binary) when the .exe form is absent.
# $1 = jdk home; sets JAVAC and HS.
resolve_jdk() {
  JAVAC="$1/bin/javac.exe"; HS="$1/bin/java.exe"
  [ -x "$JAVAC" ] || { JAVAC="$1/bin/javac"; HS="$1/bin/java"; }
}

# Strict mode is inferred from CRATONVM_ARGS, never from a build feature. It
# selects which expectation set applies (see "Expectations" below).
case " $CRATONVM_ARGS " in
  *" --jdk-only "*) MODE=jdk-only ;;
  *)                MODE=compatible ;;
esac

# Default test set — the reliably-green, fast baseline. Add classes here as the
# suite grows. RConcurrent is intentionally NOT in the default set: it exercises
# heavy multi-threaded execution, which intermittently trips a documented
# CratonVM gap (cross-thread JIT-frame root scanning at a STW GC pause — see
# README "Known gaps"), so it flakes. Run it explicitly once that gap is closed:
#   ONLY="RConcurrent" bash regression-suite/run.sh
CLASSES_CORE="RCollections RStrings RNumbers RSerial RCrypto RExceptions RReflect ROptionalClassForName RPrivateLambdaOwner RLambdaDefaultOverload RJitGc RJitStringLayout RJitArrayTypecheck RExecutorShutdown RChannelInterrupt RSocketChannelInterrupt RAtomicArray RDirectBufferElem"

# The JDK-only corpus. One vector per blocker row of
# docs/jdk-only-runtime-services.md; the mapping is in jdk-only-coverage.txt.
# Every class here is mode-INDEPENDENT: correct under --real-jdk and
# --jdk-only alike, so it is a valid HotSpot diff either way.
CLASSES_JDKONLY="RJdkHello RJdkCollections RJdkRecords RJdkLambdas RJdkHandles RJdkProxy RJdkHidden RJdkReflect RJdkJmx RJdkServices RJdkModule RJdkExecutors RJdkForkJoin RJdkAqs RJdkProcess RJdkNio RJdkNet RJdkSecurity RJdkJni RJdkFailure"

# Mode-DIVERGENT vectors: they assert the strict-mode outcome, which compatible
# mode is allowed to differ from by design (it fabricates compatibility
# classes). Scheduled only when CRATONVM_ARGS actually names --jdk-only.
CLASSES_STRICT_ONLY="RJdkStrict"

# Classes that need the named module on the module path. Skipped, with a
# message, when the module could not be built.
NEEDS_MODULE_PATH="RJdkModule"

if [ -n "$ONLY" ]; then
  CLASSES="$ONLY"
else
  case "$SUITE" in
    core)     CLASSES="$CLASSES_CORE" ;;
    jdk-only) CLASSES="$CLASSES_JDKONLY" ;;
    all)      CLASSES="$CLASSES_CORE $CLASSES_JDKONLY" ;;
    *) echo "ERROR: SUITE must be core, jdk-only or all (got '$SUITE')"; exit 3 ;;
  esac
  if [ "$MODE" = jdk-only ] && [ "$SUITE" != core ]; then
    CLASSES="$CLASSES $CLASSES_STRICT_ONLY"
  fi
fi

[ -x "$CV" ] || { echo "ERROR: CratonVM binary not found: $CV (build with build-cpu.bat)"; exit 3; }
resolve_jdk "$JDK"
[ -x "$JAVAC" ] || { echo "ERROR: javac not found: $JAVAC (set JDK=...)"; exit 3; }

# Does the selected class list contain $1?
selected() {
  case " $CLASSES " in *" $1 "*) return 0 ;; *) return 1 ;; esac
}

# Extract only the deterministic test lines (PASS/CK), stripping CratonVM's
# timestamped WARN/tracing noise and ANSI colour, so the cross-VM diff is clean.
extract() { sed 's/\x1b\[[0-9;]*m//g' | grep -aE '^(PASS|CK) ' ; }

# ---------------------------------------------------------------------------
# Compilation
# ---------------------------------------------------------------------------

MODULES_OK=0
MODULES_WHY=""

# $1 = --release value ("" for none). Compiles into $BUILD (+ $MBUILD).
compile_suite() {
  local rel="$1"
  local relflag=()
  [ -n "$rel" ] && relflag=(--release "$rel")

  rm -rf "$BUILD" "$MBUILD"; mkdir -p "$BUILD"

  # RJdkModule is compiled separately: it needs the module path, and dragging
  # that onto the whole-suite javac line would change the core compile.
  local srcs
  srcs=$(ls "$HERE"/src/*.java | grep -v '/RJdkModule\.java$')
  # shellcheck disable=SC2086
  "$JAVAC" "${relflag[@]}" -d "$BUILD" $srcs || { echo "ERROR: javac failed"; return 1; }

  # Class-path resources (META-INF/services/... for RJdkServices) are staged
  # into the build directory so discovery goes through the real resource path.
  if [ -d "$RESOURCES" ]; then
    cp -r "$RESOURCES"/. "$BUILD"/ || { echo "ERROR: staging resources failed"; return 1; }
  fi

  MODULES_OK=0; MODULES_WHY="not requested"
  if selected RJdkModule && [ -d "$MODULES_SRC/$SVC_MODULE" ]; then
    mkdir -p "$MBUILD"
    if "$JAVAC" -nowarn "${relflag[@]}" -d "$MBUILD" \
        --module-source-path "$MODULES_SRC" -m "$SVC_MODULE" >/dev/null 2>&1; then
      # Non-.java files in the module tree are module resources; stage them
      # alongside the classes so the encapsulation assertions have something
      # to read.
      ( cd "$MODULES_SRC" && find . -type f ! -name '*.java' -print ) | while read -r f; do
        mkdir -p "$MBUILD/$(dirname "$f")"
        cp "$MODULES_SRC/$f" "$MBUILD/$f"
      done
      if "$JAVAC" -nowarn "${relflag[@]}" -d "$BUILD" -p "$MBUILD" \
          --add-modules "$SVC_MODULE" "$HERE/src/RJdkModule.java" >/dev/null 2>&1; then
        MODULES_OK=1; MODULES_WHY=""
      else
        MODULES_WHY="RJdkModule.java did not compile against the module path"
      fi
    else
      MODULES_WHY="module $SVC_MODULE did not compile"
    fi
  fi
  return 0
}

# ---------------------------------------------------------------------------
# Per-class launch arguments (identical for CratonVM and HotSpot)
# ---------------------------------------------------------------------------

EXTRA=()
set_extra_args() {
  EXTRA=()
  case "$1" in
    RJdkModule) EXTRA=(-p "$MBUILD" --add-modules "$SVC_MODULE") ;;
  esac
}

# Reasons a class cannot run right now. Echoes the reason, or nothing.
skip_reason() {
  case " $NEEDS_MODULE_PATH " in
    *" $1 "*) [ "$MODULES_OK" = 1 ] || { printf 'module path unavailable: %s' "$MODULES_WHY"; return; } ;;
  esac
  case " $CLASSES_STRICT_ONLY " in
    *" $1 "*) [ "$MODE" = jdk-only ] || printf 'strict-only vector; needs CRATONVM_ARGS="--jdk-only"' ;;
  esac
}

# ---------------------------------------------------------------------------
# One pass over the class list
# ---------------------------------------------------------------------------

pass=0; fail=0; skip=0; failed=""

run_pass() {
  local jdkhome="$1"
  local c cvout cvrc cvkey hskey state why sig expfile
  for c in $CLASSES; do
    why="$(skip_reason "$c")"
    if [ -n "$why" ]; then
      skip=$((skip+1)); printf "  %-14s SKIP  %s\n" "$c" "$why"; continue
    fi
    set_extra_args "$c"
    # shellcheck disable=SC2086
    cvout=$(CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 timeout "$TIMEOUT" "$CV" $CRATONVM_ARGS \
              --java-home "$jdkhome" -cp "$BUILD" "${EXTRA[@]}" "$c" 2>&1)
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
    # Expectations. A vector whose CORRECT outcome differs between compatible
    # and strict mode gets an explicit golden per mode in regression-suite/expect
    # ("<Class>.compatible.txt" / "<Class>.jdk-only.txt"); when one exists it is
    # authoritative for that mode and REPLACES the HotSpot oracle. When none
    # exists — the normal case — HotSpot is the oracle, exactly as before.
    expfile="$EXPECT/$c.$MODE.txt"
    if [ "$state" = PASS ] && [ -f "$expfile" ]; then
      if [ "$cvkey" != "$(cat "$expfile")" ]; then
        state=FAIL; why="output differs from expect/$c.$MODE.txt"
        printf '    --- expected (%s) ---\n%s\n    --- CratonVM ---\n%s\n' \
          "$MODE" "$(cat "$expfile")" "$cvkey" | sed 's/^/    /'
      fi
    elif [ "$state" = PASS ] && [ -x "$HS" ]; then
      # Cross-VM diff against HotSpot (when present).
      # shellcheck disable=SC2086
      hskey=$(timeout "$TIMEOUT" "$HS" -cp "$BUILD" "${EXTRA[@]}" "$c" 2>&1 | extract)
      if [ "$cvkey" != "$hskey" ]; then
        state=FAIL; why="output differs from HotSpot"
        printf '    --- HotSpot ---\n%s\n    --- CratonVM ---\n%s\n' "$hskey" "$cvkey" | sed 's/^/    /'
      fi
    fi
    if [ "$state" = PASS ]; then pass=$((pass+1)); printf "  %-14s PASS\n" "$c"
    else fail=$((fail+1)); failed="$failed $c"; printf "  %-14s FAIL  %s\n" "$c" "$why"; fi
  done
}

# ---------------------------------------------------------------------------
# Driver: one pass, or the --release matrix
# ---------------------------------------------------------------------------

# Is "$JAVAC --release $1" usable? Probes with a throwaway compile so the answer
# reflects the actual toolchain rather than a version-string guess.
release_supported() {
  # NB: the probe directory must live under $HERE, not $TMPDIR. The suite runs
  # with MSYS_NO_PATHCONV=1, so an MSYS-style /tmp/... path would be handed to
  # a *Windows* javac verbatim and every release would look unsupported.
  local probe="$HERE/.release-probe.$$"
  rm -rf "$probe"; mkdir -p "$probe" || return 1
  printf 'public class CratonvmReleaseProbe { public static void main(String[] a) { } }\n' \
    > "$probe/CratonvmReleaseProbe.java"
  "$JAVAC" -nowarn --release "$1" -d "$probe" "$probe/CratonvmReleaseProbe.java" >/dev/null 2>&1
  local rc=$?
  rm -rf "$probe"
  return $rc
}

if [ -z "$RELEASES" ]; then
  echo "== compiling regression-suite =="
  compile_suite "" || exit 3
  run_pass "$JDK"
else
  for rel in $RELEASES; do
    # A per-release JDK home wins over "$JDK plus --release".
    eval "relhome=\${JDK$rel:-}"
    if [ -n "$relhome" ]; then
      if [ ! -d "$relhome" ]; then
        echo "== release $rel: SKIP (JDK$rel=$relhome does not exist) =="
        skip=$((skip+1)); continue
      fi
      resolve_jdk "$relhome"
      runhome="$relhome"
    else
      resolve_jdk "$JDK"
      runhome="$JDK"
    fi
    if [ ! -x "$JAVAC" ]; then
      echo "== release $rel: SKIP (no javac under ${relhome:-$JDK}) =="
      skip=$((skip+1)); continue
    fi
    if ! release_supported "$rel"; then
      echo "== release $rel: SKIP (javac under ${relhome:-$JDK} cannot target --release $rel) =="
      skip=$((skip+1)); continue
    fi
    echo "== compiling regression-suite (--release $rel) =="
    compile_suite "$rel" || { fail=$((fail+1)); failed="$failed release-$rel"; continue; }
    run_pass "$runhome"
  done
fi

echo "---------------------------------------------"
if [ "$skip" -gt 0 ]; then
  echo "REGRESSION SUITE: $pass passed, $fail failed, $skip skipped${failed:+ ( failed:$failed )}"
else
  echo "REGRESSION SUITE: $pass passed, $fail failed${failed:+ ( failed:$failed )}"
fi
[ "$fail" -eq 0 ]
