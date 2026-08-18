#!/usr/bin/env bash
# Run the output-side instrument guards of harness-guard.sh (G1-G4, G6) over
# every SCHEDULED vector, using HotSpot alone.
#
# This exists as its own entry point for two reasons.
#
# 1. It needs NO CratonVM binary. run.sh refuses to start without one, so on a
#    lane that cannot build the VM — which is most lanes that touch the suite —
#    the guards would otherwise be unrunnable, and an unrunnable guard is
#    indistinguishable from an absent one. It is also cheap enough for CI to
#    run on a plain JDK image.
#
# 2. It makes the guards MUTATION-TESTABLE. `MUTATE=<Class>:<sed-expr>` copies
#    src/ to a scratch tree, applies the expression to that one vector, and runs
#    the guards against the mutant, so "this guard would fire" can be measured
#    instead of asserted. The evidence for each guard is in
#    W7-60-harness-extract-blindness.md.
#
# Env: JDK=<jdk home>  ONLY="RJitGc RCrypto"  SUITE=core|jdk-only|all
#      MUTATE="RFoo:s/CK RFoo/EVIDENCE RFoo/"
#      KEEP=1   leave the scratch build tree in place for inspection
#
# 3. It is where the LAUNCH CONFIGURATION is checked against run.sh's. This
#    script used to launch every vector with a bare `-cp "$WORK/cls"` and one
#    hard-coded RJdkModule special case, while run.sh grew class_args() and
#    class_cp_extra() hooks; the first vector that needed one
#    (RServiceLoaderDoubleSource) was correctly wired in the suite and flagged
#    G4+G3 here, i.e. the instrument-check reported a defect the tree did not
#    have. Both scripts now call the SHARED hooks in harness-guard.sh, and
#    harness_hooks_drift() holds them equal to run.sh's remaining copies until
#    those are deleted.
set +e
export MSYS2_ARG_CONV_EXCL='*'; export MSYS_NO_PATHCONV=1

# MSYS_NO_PATHCONV is set above, so every path handed to javac/java must already
# be in the drive-letter form. `pwd` under Git Bash answers `/c/craton/...`,
# which javac reads as a relative path off the current drive root and cannot
# find — hence the same `git rev-parse --show-toplevel` run.sh uses.
ROOT="$(git -C "$(dirname "${BASH_SOURCE[0]}")" rev-parse --show-toplevel 2>/dev/null)"
if [ -n "$ROOT" ]; then HERE="$ROOT/regression-suite"
else HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"; fi
. "$HERE/harness-guard.sh"

JDK="${JDK:-${JAVA_HOME:-C:/Program Files/Java/jdk-25}}"
JAVAC="$JDK/bin/javac.exe"; HS="$JDK/bin/java.exe"
[ -x "$JAVAC" ] || { JAVAC="$JDK/bin/javac"; HS="$JDK/bin/java"; }
[ -x "$JAVAC" ] || { echo "ERROR: javac not found under JDK=$JDK"; exit 3; }
[ -x "$HS" ]    || { echo "ERROR: java not found under JDK=$JDK";   exit 3; }
TIMEOUT="${TIMEOUT:-120}"
JDKONLY_MODULE="cratonvm.jdkonly.svc"

# The class lists are read out of run.sh rather than duplicated, so a vector
# scheduled there is a vector guarded here. Duplicating them is how the two
# would drift, and a guard that runs over a stale list is the same defect it
# was written to catch.
CORE_CLASSES=$(sed -n 's/^CORE_CLASSES="\(.*\)"$/\1/p' "$HERE/run.sh" | head -1)
JDKONLY_CLASSES=$(sed -n 's/^JDKONLY_CLASSES="\(.*\)"$/\1/p' "$HERE/run.sh" | head -1)
[ -n "$CORE_CLASSES" ] && [ -n "$JDKONLY_CLASSES" ] || {
  echo "ERROR: could not read CORE_CLASSES/JDKONLY_CLASSES out of run.sh"; exit 3; }
case "${SUITE:-all}" in
  core)     SET="$CORE_CLASSES" ;;
  jdk-only) SET="$JDKONLY_CLASSES" ;;
  all)      SET="$CORE_CLASSES $JDKONLY_CLASSES" ;;
  *) echo "ERROR: SUITE='$SUITE' is not one of core|jdk-only|all"; exit 3 ;;
esac
CLASSES="${ONLY:-$SET}"
[ -n "$(printf '%s' "$CLASSES" | tr -d ' \t')" ] || {
  echo "ERROR: no classes scheduled — nothing would be checked."; exit 3; }

WORK="$HERE/.selfcheck"
rm -rf "$WORK"; mkdir -p "$WORK/src" "$WORK/cls" "$WORK/mod" "$WORK/out" || exit 3
cp "$HERE"/src/*.java "$WORK/src/" || exit 3

if [ -n "${MUTATE:-}" ]; then
  mut_class="${MUTATE%%:*}"; mut_expr="${MUTATE#*:}"
  [ -f "$WORK/src/$mut_class.java" ] || { echo "ERROR: MUTATE names no vector: $mut_class"; exit 3; }
  # A mutation that changed nothing produces a green run that looks like
  # evidence and is not — the whole failure mode this file is about. So the
  # no-op check has to be right, and until 2026-08-13 it was NOT: it was
  # `cmp -s <mutant> "$HERE/src/$c.java"`, and on Git Bash `sed -i` REWRITES THE
  # FILE WITH LF ENDINGS whatever the expression did. Measured on this tree:
  # RArrayStoreTiers.java 16534 bytes -> 16190 after `sed -i s/ZZ_NOT_PRESENT/x/`,
  # exactly one byte per line. So cmp differed for every mutant, the guard could
  # not fire on the platform the suite is usually run from, and
  # `MUTATE='RFoo:s/typo/x/'` reported a clean run as a mutation control.
  #
  # Compare the CONTENT, normalised for exactly that rewrite, and against the
  # pre-sed state rather than the source tree — a guard whose own precondition
  # is a platform behaviour is a guard with a platform-shaped hole in it.
  mut_before=$(tr -d '\r' < "$WORK/src/$mut_class.java" | md5sum)
  sed -i "$mut_expr" "$WORK/src/$mut_class.java" || exit 3
  mut_after=$(tr -d '\r' < "$WORK/src/$mut_class.java" | md5sum)
  if [ "$mut_before" = "$mut_after" ]; then
    echo "ERROR: MUTATE='$MUTATE' left $mut_class.java UNCHANGED — the mutant is a no-op,"
    echo "       so a green run below would be evidence of nothing. Check the expression."
    exit 3
  fi
  echo "== MUTANT: $mut_class  ($mut_expr)"
fi

copy_tree() {
  [ -d "$1" ] || return 0
  find "$1" -type f ! -name '*.java' | while IFS= read -r f; do
    rel=${f#"$1"/}; mkdir -p "$2/$(dirname "$rel")"; cp "$f" "$2/$rel"
  done
}

HAVE_MODULE=""
if [ -f "$HERE/modules/$JDKONLY_MODULE/module-info.java" ]; then
  if "$JAVAC" --module-source-path "$HERE/modules" -d "$WORK/mod" --module "$JDKONLY_MODULE"; then
    # Second pass, mirroring run.sh's `compile_modules`: recompile
    # modules-overlay/ over the module output on a PLAIN CLASSPATH, so the
    # illegal `provider()` return type javac refuses inside a `provides` clause
    # is what the VM actually loads. Without it RJdkModule's negative
    # ServiceLoader checks read green here and red under run.sh.
    if [ -d "$HERE/modules-overlay/$JDKONLY_MODULE" ]; then
      ovl=$(find "$HERE/modules-overlay/$JDKONLY_MODULE" -name '*.java' | tr '\n' ' ')
      # Unquoted on purpose: a source-file word list, not one path.
      [ -n "$ovl" ] && { "$JAVAC" -classpath "$WORK/mod/$JDKONLY_MODULE" \
          -d "$WORK/mod/$JDKONLY_MODULE" $ovl || { echo "ERROR: overlay javac failed"; exit 3; }; }
    fi
    copy_tree "$HERE/modules/$JDKONLY_MODULE" "$WORK/mod/$JDKONLY_MODULE"
    HAVE_MODULE=1
  fi
fi
# The shared hooks are written against $MODBUILD, which is run.sh's name for the
# compiled-module directory. Bind it to this script's equivalent so ONE
# definition serves both — rather than teaching the hooks a second variable.
MODBUILD="$WORK/mod"
jc_mod=""; [ -n "$HAVE_MODULE" ] && jc_mod="--module-path $MODBUILD --add-modules $JDKONLY_MODULE"
"$JAVAC" $jc_mod -d "$WORK/cls" "$WORK"/src/*.java || { echo "ERROR: javac failed"; exit 3; }
copy_tree "$HERE/resources" "$WORK/cls"

harness_load_uncounted "$HERE/harness-uncounted.txt"

# Before guarding a single vector, prove that the configuration this script
# launches them in is the configuration run.sh launches them in. A guard that
# runs the vector differently from the suite reports on a different experiment.
HARNESS_GUARD_MSGS=""; hookdrift=0
harness_hooks_drift "$HERE/run.sh" "$CORE_CLASSES $JDKONLY_CLASSES" "$WORK/hooks" || {
  hookdrift=1; printf '%s\n' "$HARNESS_GUARD_MSGS"; }

ok=0; bad=0; badlist=""
for c in $CLASSES; do
  [ -f "$WORK/src/$c.java" ] || { echo "  LIST ERROR: src/$c.java does not exist"; bad=$((bad+1)); badlist="$badlist missing:$c"; continue; }
  # class_args + class_cp_extra come from harness-guard.sh — ONE definition,
  # shared with run.sh. $extra and $cpx are intentionally unquoted: a flag word
  # list and a class-path suffix that already carries its own separator.
  extra=$(class_args "$c")
  cpx=$(class_cp_extra "$c")
  timeout "$TIMEOUT" "$HS" $extra -cp "$WORK/cls$cpx" "$c" > "$WORK/out/$c.raw" 2>&1
  rc=$?
  extract < "$WORK/out/$c.raw" > "$WORK/out/$c.key"
  HARNESS_GUARD_MSGS=""
  harness_guard_oracle "$c" "$WORK/out/$c.raw" "$rc"; r1=$?
  harness_guard_extract "$c" "$WORK/out/$c.key"; r2=$?
  if [ "$r1" -eq 0 ] && [ "$r2" -eq 0 ]; then
    ok=$((ok+1))
  else
    bad=$((bad+1)); badlist="$badlist $c"
    printf '%s\n' "$HARNESS_GUARD_MSGS"
  fi
done

[ -n "${KEEP:-}" ] || rm -rf "$WORK"
echo "---------------------------------------------"
# Reported on its own line and NOT folded into $bad: hook drift is one fact
# about two files, not a per-vector flag, and summing it into the vector count
# would make "N flagged" mean two different things. It is fatal either way.
[ "$hookdrift" -eq 0 ] || echo "HARNESS SELF-CHECK: launch hooks DRIFTED from run.sh (see [H1] above)"
echo "HARNESS SELF-CHECK: $ok vectors sound, $bad flagged${badlist:+ (${badlist# })}"
[ "$bad" -eq 0 ] && [ "$hookdrift" -eq 0 ]
