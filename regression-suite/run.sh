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
#   CRATONVM_ARGS="--jdk-only"
#       Extra launcher flags forwarded to every CratonVM invocation. Expanded
#       UNQUOTED on purpose so several flags work:
#           CRATONVM_ARGS="--jdk-only --trace-jdk-only" bash regression-suite/run.sh
#       HotSpot is deliberately NOT given them: these are CratonVM spellings
#       and the oracle has to stay the plain reference run. Without this hook a
#       CI step that exports the variable silently runs the DEFAULT policy, and
#       its green result says nothing about the policy it claimed to test.
#
#   JDK_ONLY=1
#       Also run the RJdk* JDK-only corpus (src/RJdk*.java, the named module
#       under modules/, and the class-path service resources under resources/).
#       Implied when CRATONVM_ARGS names --jdk-only.
#
#   RELEASES="17 21 25"
#       Compile and run the suite once per `javac --release` level instead of
#       once with the default target. A level with no usable javac is SKIPPED
#       with a message, never silently dropped; a run in which every level was
#       skipped exits non-zero, because "nothing ran" must not read as green.
#       Per-level JDK homes come from JDK17 / JDK21 / JDK25 when set, otherwise
#       from $JDK when that javac can target the level. Unset (the default) is
#       a single pass with no --release — the historical behaviour.
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

# JDK-only corpus inputs. `modules/` holds a real named module compiled to
# `build-modules/` and put on the module path; `resources/` holds real
# META-INF/services provider files copied into the class-path build directory,
# so ServiceLoader discovery goes through ClassLoader.getResources rather than
# a fabricated shortcut.
MODSRC="$HERE/modules"
RESOURCES="$HERE/resources"
JDKONLY_MODULE="cratonvm.jdkonly.svc"

# Default test set — the reliably-green, fast baseline. Add classes here as the
# suite grows. RConcurrent is intentionally NOT in the default set: it exercises
# heavy multi-threaded execution, which intermittently trips a documented
# CratonVM gap (cross-thread JIT-frame root scanning at a STW GC pause — see
# README "Known gaps"), so it flakes. Run it explicitly once that gap is closed:
#   ONLY="RConcurrent" bash regression-suite/run.sh
CORE_CLASSES="RCollections RStrings RNumbers RSerial RCrypto RExceptions RReflect ROptionalClassForName RPrivateLambdaOwner RLambdaDefaultOverload RJitGc RJitStringLayout RJitArrayTypecheck RExecutorShutdown RChannelInterrupt RSocketChannelInterrupt RAtomicArray RDirectBufferElem RMapResizeGc RMapGcStress RForNameGcStress ROverlaySystemGcStress"

# The JDK-only corpus (docs/feature-designs/jdk-only-mode.md). Not in the
# default set: `--jdk-only` is an internal-diagnostic policy in wave 1 and is
# *expected* to fail where --real-jdk passes, so these must not move the green
# baseline of a plain `bash regression-suite/run.sh`.
JDKONLY_CLASSES="RJdkHello RJdkStrict RJdkCollections RJdkLambdas RJdkHandles RJdkProxy RJdkReflect RJdkRecords RJdkHidden RJdkModule RJdkServices RJdkAqs RJdkExecutors RJdkForkJoin RJdkNio RJdkNet RJdkProcess RJdkSecurity RJdkJmx RJdkJni RJdkFailure"
# Keep the list to vectors that actually exist on disk, so adding/removing a
# source file does not silently turn into a "no PASS line" failure.
present=""
for c in $JDKONLY_CLASSES; do
  [ -f "$HERE/src/$c.java" ] && present="$present $c"
done
JDKONLY_CLASSES="${present# }"

# `--jdk-only` in CRATONVM_ARGS implies the JDK-only corpus. The trailing space
# in the pattern keeps `--jdk-only-report <FILE>` from matching on its own.
case " ${CRATONVM_ARGS:-} " in
  *" --jdk-only "*) JDK_ONLY=1 ;;
esac
CLASSES="${ONLY:-$CORE_CLASSES${JDK_ONLY:+ $JDKONLY_CLASSES}}"

[ -x "$CV" ] || { echo "ERROR: CratonVM binary not found: $CV (build with build-cpu.bat)"; exit 3; }
[ -x "$JAVAC" ] || { echo "ERROR: javac not found: $JAVAC (set JDK=...)"; exit 3; }

# Extract only the deterministic test lines (PASS/CK), stripping CratonVM's
# timestamped WARN/tracing noise and ANSI colour, so the cross-VM diff is clean.
extract() { sed 's/\x1b\[[0-9;]*m//g' | grep -aE '^(PASS|CK) ' ; }

# Copy every non-source file under $1 into $2, preserving relative paths.
copy_tree() {
  [ -d "$1" ] || return 0
  find "$1" -type f ! -name '*.java' | while IFS= read -r f; do
    rel=${f#"$1"/}
    mkdir -p "$2/$(dirname "$rel")"
    cp "$f" "$2/$rel"
  done
}

# Compile the named module into $MODBUILD and copy its encapsulated resources
# next to the class files. Sets HAVE_MODULE when the module is usable.
compile_modules() {
  HAVE_MODULE=""
  [ -f "$MODSRC/$JDKONLY_MODULE/module-info.java" ] || {
    echo "  NOTE: $MODSRC/$JDKONLY_MODULE/module-info.java absent — module vectors will not link"
    return 0
  }
  rm -rf "$MODBUILD"; mkdir -p "$MODBUILD" || return 1
  # `--module <name>` compiles the whole module off the module source path, so
  # no source list has to be word-split here.
  if [ -n "$REL" ]; then
    "$JAVAC" --release "$REL" --module-source-path "$MODSRC" -d "$MODBUILD" --module "$JDKONLY_MODULE" || return 1
  else
    "$JAVAC" --module-source-path "$MODSRC" -d "$MODBUILD" --module "$JDKONLY_MODULE" || return 1
  fi
  copy_tree "$MODSRC/$JDKONLY_MODULE" "$MODBUILD/$JDKONLY_MODULE"
  HAVE_MODULE=1
  return 0
}

# Compile src/*.java into $BUILD and stage the class-path service resources.
compile_suite() {
  rm -rf "$BUILD"; mkdir -p "$BUILD" || return 1
  jc_rel=""; [ -n "$REL" ] && jc_rel="--release $REL"
  jc_mod=""; [ -n "$HAVE_MODULE" ] && jc_mod="--module-path $MODBUILD --add-modules $JDKONLY_MODULE"
  # Unquoted on purpose (option words, not one path). $MODBUILD lives under the
  # repository root, so it carries no spaces.
  "$JAVAC" $jc_rel $jc_mod -d "$BUILD" "$HERE"/src/*.java || return 1
  copy_tree "$RESOURCES" "$BUILD"
  return 0
}

# Launcher arguments a specific vector needs on top of CRATONVM_ARGS. Emitted
# as a word list, consumed unquoted.
class_args() {
  case "$1" in
    RJdkModule)
      [ -n "$HAVE_MODULE" ] && printf '%s' "--module-path $MODBUILD --add-modules $JDKONLY_MODULE"
      ;;
    *) : ;;
  esac
}

# Can $1 (a javac) actually target `--release $2`? Probed with a throwaway
# compile rather than by parsing --help: javac accepts the option and then
# rejects the value, and a stale ct.sym makes the answer machine-specific.
# Probing here keeps an unsupported level distinguishable from a genuine source
# error in the suite compile.
release_supported() {
  probe="$HERE/.release-probe.$2"
  rm -rf "$probe"; mkdir -p "$probe" || return 1
  printf 'public class RelProbe { public static void main(String[] a) { } }\n' > "$probe/RelProbe.java"
  "$1" --release "$2" -d "$probe" "$probe/RelProbe.java" >/dev/null 2>&1
  probe_rc=$?
  rm -rf "$probe"
  return $probe_rc
}

# Resolve the javac/java/JDK home for one --release level. Returns 1 (skip)
# when nothing on this machine can target it.
resolve_release() {
  # The per-level home is read as ${JDK<level>}, so the level has to be a bare
  # feature number; anything else would break the indirection rather than skip.
  case "$1" in
    ''|*[!0-9]*) echo "   (release level '$1' is not a bare feature number)"; return 1 ;;
  esac
  eval "rel_home=\${JDK$1:-}"
  if [ -n "$rel_home" ]; then
    R_HOME="$rel_home"
    R_JAVAC="$rel_home/bin/javac.exe"; R_HS="$rel_home/bin/java.exe"
    [ -x "$R_JAVAC" ] || { R_JAVAC="$rel_home/bin/javac"; R_HS="$rel_home/bin/java"; }
    [ -x "$R_JAVAC" ] || return 1
    release_supported "$R_JAVAC" "$1" || return 1
    return 0
  fi
  # No dedicated home: the configured javac may still be able to target the
  # level through ct.sym.
  release_supported "$JAVAC" "$1" || return 1
  R_HOME="$JDK"; R_JAVAC="$JAVAC"; R_HS="$HS"
  return 0
}

# Compile once and run $CLASSES. Reads REL/BUILD/MODBUILD/JAVAC/HS/JDK; sets
# pass/fail/failed.
run_pass() {
  pass=0; fail=0; failed=""
  label=""; [ -n "$REL" ] && label=" (--release $REL)"
  echo "== compiling regression-suite$label =="
  compile_modules || { echo "ERROR: javac failed on module $JDKONLY_MODULE"; return 3; }
  compile_suite   || { echo "ERROR: javac failed"; return 3; }

  for c in $CLASSES; do
    extra=$(class_args "$c")
    # $CRATONVM_ARGS and $extra are intentionally unquoted: both are flag
    # lists, not single paths.
    cvout=$(CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 timeout "$TIMEOUT" "$CV" --java-home "$JDK" ${CRATONVM_ARGS:-} $extra -cp "$BUILD" "$c" 2>&1)
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
    # Cross-VM diff against HotSpot (when present). HotSpot gets the vector's
    # own arguments but never CRATONVM_ARGS — the oracle must stay unmodified.
    if [ "$state" = PASS ] && [ -x "$HS" ]; then
      hskey=$(timeout "$TIMEOUT" "$HS" $extra -cp "$BUILD" "$c" 2>&1 | extract)
      if [ "$cvkey" != "$hskey" ]; then
        state=FAIL; why="output differs from HotSpot"
        printf '    --- HotSpot ---\n%s\n    --- CratonVM ---\n%s\n' "$hskey" "$cvkey" | sed 's/^/    /'
      fi
    fi
    if [ "$state" = PASS ]; then pass=$((pass+1)); printf "  %-14s PASS\n" "$c"
    else fail=$((fail+1)); failed="$failed $c"; printf "  %-14s FAIL  %s\n" "$c" "$why"; fi
  done
  return 0
}

total_pass=0; total_fail=0; total_failed=""; ran=0; skipped=""

if [ -z "${RELEASES:-}" ]; then
  REL=""
  MODBUILD="$HERE/build-modules"
  run_pass; rc=$?
  [ "$rc" -eq 0 ] || exit "$rc"
  ran=1
  total_pass=$pass; total_fail=$fail; total_failed="$failed"
else
  BASE_JDK="$JDK"; BASE_JAVAC="$JAVAC"; BASE_HS="$HS"
  for REL in $RELEASES; do
    JDK="$BASE_JDK"; JAVAC="$BASE_JAVAC"; HS="$BASE_HS"
    if ! resolve_release "$REL"; then
      echo "== SKIP --release $REL: no javac on this machine can target it"
      echo "   (set JDK$REL=<jdk home>, or use a JDK whose javac supports --release $REL)"
      skipped="$skipped $REL"
      continue
    fi
    JDK="$R_HOME"; JAVAC="$R_JAVAC"; HS="$R_HS"
    BUILD="$HERE/build/r$REL"
    MODBUILD="$HERE/build-modules/r$REL"
    run_pass; rc=$?
    if [ "$rc" -ne 0 ]; then
      echo "  --release $REL: compile failed"
      total_fail=$((total_fail+1)); total_failed="$total_failed release-$REL"
      continue
    fi
    ran=$((ran+1))
    total_pass=$((total_pass+pass)); total_fail=$((total_fail+fail))
    [ -n "$failed" ] && total_failed="$total_failed$(printf '%s' "$failed" | sed "s/ / r$REL:/g")"
    echo "  --release $REL: $pass passed, $fail failed"
  done
  [ -n "$skipped" ] && echo "SKIPPED release levels:$skipped"
  if [ "$ran" -eq 0 ]; then
    echo "ERROR: RELEASES='$RELEASES' but no level could be compiled — nothing ran."
    exit 3
  fi
fi

echo "---------------------------------------------"
echo "REGRESSION SUITE: $total_pass passed, $total_fail failed${total_failed:+ ( failed:$total_failed )}"
[ "$total_fail" -eq 0 ]
