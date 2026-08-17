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
#   SUITE=core|jdk-only|all
#       Which class list to schedule: `core` (default, the historical set),
#       `jdk-only` (the RJdk* corpus INSTEAD of core), `all` (both). This is
#       the spelling regression-suite/README.md, CHANGELOG.md and ROADMAP.md
#       have always documented; until 2026-08 it was not implemented here at
#       all, so `SUITE=jdk-only bash run.sh` and `SUITE=all RELEASES=...`
#       quietly ran the CORE set and reported green — a documented invocation
#       whose result said nothing about the corpus it named. An unrecognised
#       value is a hard error, never a silent fall-back to core.
#
#   STRICT_COVERAGE=1
#       Make the coverage census fatal: a src/*.java vector that appears in no
#       class list (and is not named in UNREGISTERED_CLASSES with a reason)
#       counts as a failure instead of a warning. Off by default only because
#       vectors land from several branches at once; CI should set it.
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
# Sources recompiled OVER $MODBUILD once the module itself has been compiled,
# with no module context. This is how the suite expresses a provider shape javac
# REFUSES in a `provides` clause (a `provider()` whose return type is not a
# subtype of the service): javac enforces that only while compiling
# module-info.java, and `ServiceLoader.loadProvider` carries the same rule as a
# RUNTIME gate precisely for modules that were not assembled by javac. Dropping
# this pass leaves a module that passes its own NEGATIVE test, so
# `compile_modules` fails loudly rather than skipping it.
MODOVERLAY="$HERE/modules-overlay"
RESOURCES="$HERE/resources"
JDKONLY_MODULE="cratonvm.jdkonly.svc"

# Default test set — the reliably-green, fast baseline. Add classes here as the
# suite grows. RConcurrent is intentionally NOT in the default set: it exercises
# heavy multi-threaded execution, which intermittently trips a documented
# CratonVM gap (cross-thread JIT-frame root scanning at a STW GC pause — see
# README "Known gaps"), so it flakes. Run it explicitly once that gap is closed:
#   ONLY="RConcurrent" bash regression-suite/run.sh
#
# Two members — RPriorityQueueGc and RTreeRangeGc — are INERT without the
# CratonVM-only launcher flags class_cv_args() supplies for them. Scheduling
# either one without that hook manufactures a green gate; see class_cv_args.
#
# RJdkViews is in CORE despite its RJdk* name. The RJdk* prefix is the JDK-only
# corpus's naming convention, but that corpus is about `--jdk-only` POLICY, and
# RJdkViews asserts `--real-jdk` COMPATIBILITY behaviour — TreeMap navigable
# views, the Iterator.remove state machine, %e/%g float formatting,
# StringBuilder.delete bounds, IntStream.summaryStatistics. That is a default-
# mode concern, so it belongs to the set a plain `bash run.sh` runs. It is
# deliberately NOT also in JDKONLY_CLASSES: under CRATONVM_ARGS="--jdk-only"
# every scheduled class already receives that flag, so a second registration
# would run the identical command twice. Nothing schedules by glob — every list
# here is explicit — so the name collision is cosmetic.
CORE_CLASSES="RCollections RStrings RNumbers RSerial RCrypto RExceptions RReflect ROptionalClassForName RPrivateLambdaOwner RLambdaDefaultOverload RJitGc RJitStringLayout RJitArrayTypecheck RArraysMismatch RExecutorShutdown RBlockingQueue RChmKeySetView RChannelInterrupt RSocketChannelInterrupt RAtomicArray RDirectBufferElem RMapResizeGc RMapGcStress RForNameGcStress ROverlaySystemGcStress RFileTimes RNioNoFollow RSyncMethodJit RFieldSiteCache RMethodSiteCache RDataInputFastPull RCanAccessRules RChaCha20Cipher RLockedIdentityHash RCanAccessReceiver RForeignLayoutCollections RForeignLayoutJdkInterfaces RLoaderChurnDefine RClassUnloadSweep RClassUnloadSweepGen RPriorityQueueGc RTreeRangeGc RJdkViews RVarHandleAccess"

# The JDK-only corpus (docs/feature-designs/jdk-only-mode.md). Not in the
# default set: `--jdk-only` is an internal-diagnostic policy in wave 1 and is
# *expected* to fail where --real-jdk passes, so these must not move the green
# baseline of a plain `bash regression-suite/run.sh`.
#
# RJdkStampedStamps / RJdkLookupIn / RJdkDefineClass landed 2026-08-11 as the
# executable form of the three predictions in the native-precedence re-audit
# that no existing vector covered. Each asks a question only an UNREGISTERED
# decoder can answer — the JDK's own static stamp predicates, dropLookupMode,
# a defined class read back through java.lang.Class — so a registered surface
# agreeing with itself cannot make them pass.
JDKONLY_CLASSES="RJdkHello RJdkStrict RJdkCollections RJdkLambdas RJdkHandles RJdkProxy RJdkReflect RJdkFieldModule RJdkRecords RJdkHidden RJdkModule RJdkServices RJdkAqs RJdkPhaser RJdkExecutors RJdkForkJoin RJdkNio RJdkNet RJdkProcess RJdkSecurity RJdkJmx RJdkJni RJdkFailure RJdkStampedStamps RJdkLookupIn RJdkDefineClass RJdkX509Intercept RJdkLogging"

# Vectors that deliberately belong to NO class list. Every entry needs a
# reason, because "not scheduled" is indistinguishable from "forgotten" once
# the reason is only in someone's head.
#
#   RConcurrent      heavy multi-threaded execution; trips the documented
#                    cross-thread JIT-frame root-scan gap (README "Known
#                    gaps") and flakes. Run with ONLY="RConcurrent".
#   RCanAccessOutsider
#                    not a vector: it has no main. It is the FOREIGN-package
#                    half of RCanAccessRules, which lives in the unnamed
#                    package and therefore cannot be named from any other
#                    package — the "caller is not in the declaring class's
#                    runtime package" arm has to be asked from a file that
#                    declares one.
#
# RPriorityQueueGc and RTreeRangeGc were HERE until 2026-08-11 and are now in
# CORE_CLASSES. History, because it is the whole reason they were absent: both
# were in the runner's class list with a cv_extra_args hook when their fixes
# landed (6cd01bcba, b2e13e441), and both were validated FAIL-then-PASS under
# it. A later run.sh merge resolution silently discarded the registrations AND
# the hook. The hook came back first, as class_cv_args() below, but the two
# classes stayed parked here with a note saying scheduling them was "a task for
# a lane that can build and run the VM" — so the repair was half-done and the
# two documented permanent gates for a heap-corruption defect still never ran.
#
# The scheduling half is now done too. The ordering mattered and is worth
# stating: re-registering these WITHOUT class_cv_args would have been worse
# than leaving them out, because both pass on a broken VM without their flags
# (see class_cv_args) and the suite would have gained two green-forever vectors
# that look like coverage. The hook is in place first; the registration second.
#
# NOT YET VERIFIED against a CratonVM binary — this lane could not build or run
# one. Both pass on HotSpot 25 with byte-identical output over repeated runs, so
# they are sound vectors, but the first CratonVM run of them under these flags
# is still ahead. If either comes back red, that is the gate doing its job and
# means the underlying fix regressed; it is not a bad registration.
UNREGISTERED_CLASSES="RConcurrent RCanAccessOutsider"

# ---- list hygiene, computed before anything is pruned --------------------
#
# Two failure modes, both of which used to read as green:
#
#   * a src/*.java vector named in no list. It compiles (compile_suite globs
#     src/*.java) and never runs, so it looks like coverage and is not. This
#     is how RJdkPhaser — 240 checks — arrived inert.
#   * a list entry whose .java is gone. The JDK-only list used to filter these
#     out silently, so renaming a vector deleted its coverage and only lowered
#     the "N passed" count, which nobody diffs.
LISTED_CLASSES="$CORE_CLASSES $JDKONLY_CLASSES"
UNREGISTERED_FOUND=""
for f in "$HERE"/src/*.java; do
  [ -f "$f" ] || continue
  b=$(basename "$f" .java)
  case " $LISTED_CLASSES $UNREGISTERED_CLASSES " in
    *" $b "*) ;;
    *) UNREGISTERED_FOUND="$UNREGISTERED_FOUND $b" ;;
  esac
done
STALE_UNREGISTERED=""
for c in $UNREGISTERED_CLASSES; do
  [ -f "$HERE/src/$c.java" ] || STALE_UNREGISTERED="$STALE_UNREGISTERED $c"
done
# Prune missing entries from what gets scheduled, but REMEMBER them: each one
# is counted as a failure in the summary below. The result comes back in
# $PRUNED rather than on stdout on purpose — `X=$(prune_missing ...)` runs the
# function in a SUBSHELL, so the MISSING_CLASSES it appends to would be
# discarded and the check would silently never fire.
MISSING_CLASSES=""
prune_missing() {
  PRUNED=""
  for c in $1; do
    if [ -f "$HERE/src/$c.java" ]; then PRUNED="$PRUNED $c"
    else MISSING_CLASSES="$MISSING_CLASSES $c"; fi
  done
  PRUNED="${PRUNED# }"
}
prune_missing "$CORE_CLASSES";    CORE_CLASSES="$PRUNED"
prune_missing "$JDKONLY_CLASSES"; JDKONLY_CLASSES="$PRUNED"

# `--jdk-only` in CRATONVM_ARGS implies the JDK-only corpus. The trailing space
# in the pattern keeps `--jdk-only-report <FILE>` from matching on its own.
case " ${CRATONVM_ARGS:-} " in
  *" --jdk-only "*) JDK_ONLY=1 ;;
esac
# SUITE selects the list; JDK_ONLY=1 stays additive on top of it.
SUITE_SET=""
case "${SUITE:-core}" in
  core)     SUITE_SET="$CORE_CLASSES${JDK_ONLY:+ $JDKONLY_CLASSES}" ;;
  jdk-only) SUITE_SET="$JDKONLY_CLASSES" ;;
  all)      SUITE_SET="$CORE_CLASSES $JDKONLY_CLASSES" ;;
  *) echo "ERROR: SUITE='$SUITE' is not one of core|jdk-only|all"; exit 3 ;;
esac
CLASSES="${ONLY:-$SUITE_SET}"
# A run that schedules nothing must not print a green summary. Reachable via
# ONLY=" ", an emptied list, or SUITE=jdk-only against a checkout with no
# RJdk* sources.
if [ -z "$(printf '%s' "$CLASSES" | tr -d ' \t')" ]; then
  echo "ERROR: no classes scheduled (SUITE=${SUITE:-core} ONLY='${ONLY:-}') — nothing would run."
  exit 3
fi

[ -x "$CV" ] || { echo "ERROR: CratonVM binary not found: $CV (build with build-cpu.bat)"; exit 3; }
[ -x "$JAVAC" ] || { echo "ERROR: javac not found: $JAVAC (set JDK=...)"; exit 3; }

# extract() — the PASS/CK filter the cross-VM diff runs through — and the four
# guards that keep it from deleting the evidence, both live in harness-guard.sh.
# ONE definition, because the filter the suite diffs through and the filter the
# guards reason about drifting apart is the same defect one level up.
#
# The guards exist because for a long time nothing checked that anything
# meaningful survived this filter. Three scheduled vectors printed their entire
# evidence on other prefixes, extract() reduced each to the constant
# `PASS <Class>`, and a constant always matches itself: RDataInputFastPull with
# a one-line defect injected exited rc=0 with output byte-identical to a clean
# run. See W7-60-harness-extract-blindness.md and W7-51-vacuous-sweep-round-2.md.
. "$HERE/harness-guard.sh" || { echo "ERROR: cannot source $HERE/harness-guard.sh"; exit 3; }
harness_load_uncounted "$HERE/harness-uncounted.txt"

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
  # Second pass: recompile $MODOVERLAY over the class files just produced, on a
  # PLAIN CLASSPATH so javac never sees the `provides` clause it would reject.
  # See $MODOVERLAY's own sources for why the shape cannot be written directly.
  if [ -d "$MODOVERLAY/$JDKONLY_MODULE" ]; then
    ovl=$(find "$MODOVERLAY/$JDKONLY_MODULE" -name '*.java' | tr '\n' ' ')
    if [ -n "$ovl" ]; then
      # Unquoted on purpose: a source-file word list, and the suite tree carries
      # no spaces. `-classpath` is the already-compiled module output so the
      # overlay can still reference the module's own types.
      if [ -n "$REL" ]; then
        "$JAVAC" --release "$REL" -classpath "$MODBUILD/$JDKONLY_MODULE" \
            -d "$MODBUILD/$JDKONLY_MODULE" $ovl || return 1
      else
        "$JAVAC" -classpath "$MODBUILD/$JDKONLY_MODULE" \
            -d "$MODBUILD/$JDKONLY_MODULE" $ovl || return 1
      fi
    fi
  fi
  # Ground-truth the overlay instead of trusting that it landed. If the second
  # pass silently did nothing, RJdkModule's negative ServiceLoader checks would
  # fail on BOTH VMs — a harness error wearing a VM defect's clothes, which is
  # the exact misclassification this vector's record warns about.
  if [ -f "$MODBUILD/$JDKONLY_MODULE/com/cratonvm/jdkonly/svc/internal/WrongFactory.class" ]; then
    if ! "$JDK/bin/javap" -p -classpath "$MODBUILD/$JDKONLY_MODULE" \
        com.cratonvm.jdkonly.svc.internal.WrongFactory 2>/dev/null \
        | grep -q 'public static java.lang.Object provider()'; then
      echo "ERROR: modules-overlay did not land — WrongFactory.provider() must return Object"
      return 1
    fi
  fi
  # Same ground-truthing for the two constructor-form providers (W6-2). Each
  # names a shape javac refuses in a `provides` clause, so the overlay pass is
  # the only thing that can produce it: NotSubProvider must implement nothing,
  # and HiddenCtor's no-arg constructor must be private.
  if [ -f "$MODBUILD/$JDKONLY_MODULE/com/cratonvm/jdkonly/svc/internal/NotSubProvider.class" ]; then
    if "$JDK/bin/javap" -p -classpath "$MODBUILD/$JDKONLY_MODULE" \
        com.cratonvm.jdkonly.svc.internal.NotSubProvider 2>/dev/null \
        | grep -q 'implements'; then
      echo "ERROR: modules-overlay did not land — NotSubProvider must implement nothing"
      return 1
    fi
  fi
  if [ -f "$MODBUILD/$JDKONLY_MODULE/com/cratonvm/jdkonly/svc/internal/HiddenCtor.class" ]; then
    if ! "$JDK/bin/javap" -p -classpath "$MODBUILD/$JDKONLY_MODULE" \
        com.cratonvm.jdkonly.svc.internal.HiddenCtor 2>/dev/null \
        | grep -q 'private com.cratonvm.jdkonly.svc.internal.HiddenCtor('; then
      echo "ERROR: modules-overlay did not land — HiddenCtor() must be private"
      return 1
    fi
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

# Launcher arguments a specific vector needs, in a spelling BOTH VMs accept —
# these are handed to HotSpot too, so the oracle runs the same shape. Emitted
# as a word list, consumed unquoted.
class_args() {
  case "$1" in
    RJdkModule)
      [ -n "$HAVE_MODULE" ] && printf '%s' "--module-path $MODBUILD --add-modules $JDKONLY_MODULE"
      ;;
    *) : ;;
  esac
}

# CratonVM-ONLY arguments for a vector: launcher flags in CratonVM's own
# spelling, which HotSpot would reject outright. Kept separate from class_args
# for exactly that reason — a `--nojit` in class_args would make the oracle
# exit non-zero, its key lines come back empty, and every such vector would
# fail the cross-VM diff for a reason that has nothing to do with the VM.
#
# The two entries below are the reproduction conditions their vectors' own doc
# comments already claim the suite supplies. Both vectors are in CORE_CLASSES as
# of 2026-08-11, so these arguments are now load-bearing on every default run.
#
# BOTH GATES ARE INERT WITHOUT THEIR ARGUMENT — they do not merely lose
# sensitivity, they PASS ON A BROKEN VM, which is worse than not running at all
# because it reads as coverage. The two mechanisms, from the internal records
# fixed-suite-bugs/h2-suite-bugs/bug-h2-priorityblockingqueue-stale-objectref-classcastexception-FIXED.md
# and fixed-suite-bugs/treemap-treeset-range-snapshot-stale-objectref-FIXED.md:
#
#   * on the default heap no collection happens during the walk at all, so the
#     stale ObjectRef is never created; and
#   * with a live JIT frame on the stack the young generation falls back to a
#     non-moving sweep, under which a stale reference still resolves and the
#     defect hides entirely.
#
# Hence --Xmx 64m for both, and --nojit for RPriorityQueueGc only. RTreeRangeGc
# must NOT get --nojit: it reproduces with the JIT on (3/3, b2e13e441), so
# withholding the flag is what keeps the default compiling configuration under
# test. Do not "make the two entries consistent" by adding it.
#
# Do not drop these again. A previous merge resolution did, silently. Nothing
# caught it: a vector that stops being scheduled does not turn a run red, it
# only makes the pass count smaller, and nobody diffs the pass count. The
# UNREGISTERED_CLASSES census and the COVERAGE ERROR below exist because of
# this — they are what makes the next such drop visible.
class_cv_args() {
  case "$1" in
    RPriorityQueueGc) printf '%s' "--nojit --Xmx 64m" ;;
    RTreeRangeGc)     printf '%s' "--Xmx 64m" ;;
    # The COLLECTOR is a variable this suite otherwise never moves: every other
    # vector runs on whatever the default happens to be, and that default
    # changed (Generational -> ZGC) on 2026-08-10 with nothing scheduled to
    # notice. `RClassUnloadSweepGen` is `RClassUnloadSweep`'s own probe run
    # against the generational young sweep, where the last surviving arm of
    # `TestDefaultInstanceManager`'s fourth recurrence lived. CratonVM-only by
    # construction: `$cvextra` never reaches the HotSpot oracle, which is
    # correct here — the claim is "a real JVM unloads this class", not "under a
    # named collector".
    RClassUnloadSweepGen) printf '%s' "-XX:+UseGenerationalGC" ;;
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
  hbad=0; hfailed=""
  GUARDTMP="$HERE/.guard-tmp"; rm -rf "$GUARDTMP"; mkdir -p "$GUARDTMP"
  label=""; [ -n "$REL" ] && label=" (--release $REL)"
  echo "== compiling regression-suite$label =="
  compile_modules || { echo "ERROR: javac failed on module $JDKONLY_MODULE"; return 3; }
  compile_suite   || { echo "ERROR: javac failed"; return 3; }
  # Say so out loud. Without HotSpot only check (1)-(3) run; the byte-for-byte
  # cross-VM diff — the check that catches a miscompiled checksum — is gone,
  # and a green summary from such a run means much less than it looks like.
  [ -x "$HS" ] || echo "  NOTE: no HotSpot at $HS — cross-VM output diff SKIPPED for every class"

  for c in $CLASSES; do
    extra=$(class_args "$c")
    cvextra=$(class_cv_args "$c")
    # $CRATONVM_ARGS, $extra and $cvextra are intentionally unquoted: all three
    # are flag lists, not single paths.
    cvout=$(CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 timeout "$TIMEOUT" "$CV" --java-home "$JDK" ${CRATONVM_ARGS:-} $cvextra $extra -cp "$BUILD" "$c" 2>&1)
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
    # Anchored on a word boundary: a bare `^PASS $c` would let `PASS RJdkPhaser`
    # satisfy a check for a class named `RJdkPhase`.
    elif ! printf '%s\n' "$cvkey" | grep -qaE "^PASS $c([^A-Za-z0-9_]|\$)"; then
      state=FAIL; why="no PASS line"
    fi
    # Cross-VM diff against HotSpot (when present). HotSpot gets the vector's
    # own cross-VM arguments but never CRATONVM_ARGS and never $cvextra — the
    # oracle must stay unmodified.
    #
    # The oracle's RAW output and its EXIT CODE are both kept now. Until
    # 2026-08-12 this line was `hskey=$(... | extract)`: the rc was thrown away,
    # so an oracle that crashed or hit the 120 s timeout silently became a
    # TRUNCATED ground truth, and the raw output was thrown away, so the lines
    # extract() deleted — which is exactly the evidence the harness is blind to
    # — could not be inspected. Guards G1 and G4 both need what was discarded.
    HARNESS_GUARD_MSGS=""; guarded=0
    if [ -x "$HS" ]; then
      timeout "$TIMEOUT" "$HS" $extra -cp "$BUILD" "$c" > "$GUARDTMP/hs.raw" 2>&1
      hsrc=$?
      hskey=$(extract < "$GUARDTMP/hs.raw")
      harness_guard_oracle "$c" "$GUARDTMP/hs.raw" "$hsrc" || guarded=1
      if [ "$state" = PASS ] && [ "$cvkey" != "$hskey" ]; then
        state=FAIL; why="output differs from HotSpot"
        printf '    --- HotSpot ---\n%s\n    --- CratonVM ---\n%s\n' "$hskey" "$cvkey" | sed 's/^/    /'
      fi
    fi
    # G2/G3 read only what survived the filter, so they run even with no
    # HotSpot on the box — the run where they matter MOST, because that is the
    # run where the cross-VM diff is skipped for every class and a vector's
    # banner is the only thing left.
    printf '%s\n' "$cvkey" > "$GUARDTMP/cv.key"
    harness_guard_extract "$c" "$GUARDTMP/cv.key" || guarded=1

    if [ "$state" = PASS ]; then pass=$((pass+1)); printf "  %-14s PASS\n" "$c"
    else fail=$((fail+1)); failed="$failed $c"; printf "  %-14s FAIL  %s\n" "$c" "$why"; fi
    # Reported SEPARATELY from the vector's own verdict, and counted separately.
    # "the VM answered wrongly" and "the instrument cannot see the answer" are
    # different findings and must not be summed into one number.
    if [ "$guarded" -ne 0 ]; then
      hbad=$((hbad+1)); hfailed="$hfailed $c"
      printf '%s\n' "$HARNESS_GUARD_MSGS"
    fi
  done
  rm -rf "$GUARDTMP"
  return 0
}

total_pass=0; total_fail=0; total_failed=""; ran=0; skipped=""
total_hbad=0; total_hfailed=""

if [ -z "${RELEASES:-}" ]; then
  REL=""
  MODBUILD="$HERE/build-modules"
  run_pass; rc=$?
  [ "$rc" -eq 0 ] || exit "$rc"
  ran=1
  total_pass=$pass; total_fail=$fail; total_failed="$failed"
  total_hbad=$hbad; total_hfailed="$hfailed"
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
    total_hbad=$((total_hbad+hbad))
    [ -n "$hfailed" ] && total_hfailed="$total_hfailed$(printf '%s' "$hfailed" | sed "s/ / r$REL:/g")"
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

# ---- list hygiene, reported where the summary is actually read -----------
#
# A list entry with no source is ALWAYS a failure. It used to be filtered out
# in silence, which turned "this vector was renamed and its coverage is gone"
# into a slightly smaller pass count.
for c in $MISSING_CLASSES; do
  echo "  LIST ERROR: '$c' is in a class list but src/$c.java does not exist"
  total_fail=$((total_fail+1)); total_failed="$total_failed missing:$c"
done
for c in $STALE_UNREGISTERED; do
  echo "  LIST ERROR: '$c' is in UNREGISTERED_CLASSES but src/$c.java does not exist"
  total_fail=$((total_fail+1)); total_failed="$total_failed stale:$c"
done

# A src/*.java that no list schedules compiles and never runs. WARNING by
# default because vectors land here from several branches at once and the
# lane that lands next must not inherit someone else's red; STRICT_COVERAGE=1
# makes it fatal, which is what CI should run.
if [ -n "$UNREGISTERED_FOUND" ]; then
  for c in $UNREGISTERED_FOUND; do
    if [ -n "${STRICT_COVERAGE:-}" ]; then
      echo "  COVERAGE ERROR: src/$c.java is in no class list — it compiles and never runs"
      total_fail=$((total_fail+1)); total_failed="$total_failed unregistered:$c"
    else
      echo "  COVERAGE WARNING: src/$c.java is in no class list — it compiles and never runs."
      echo "    Add it to CORE_CLASSES or JDKONLY_CLASSES, or to UNREGISTERED_CLASSES with a"
      echo "    reason. Set STRICT_COVERAGE=1 to make this a failure."
    fi
  done
fi

# ---- the instrument's own verdict ----------------------------------------
#
# Counted into the exit status, and reported on its own line. A harness guard
# firing does NOT mean the VM is wrong; it means the run just reported on a
# comparison it could not have lost. That is the more serious of the two,
# because every other lane's "green" rests on this instrument — so it is fatal,
# not a warning. A warning inside a green build is how the previous version of
# this defect survived long enough to be measured.
if [ "$total_hbad" -gt 0 ]; then
  echo "  HARNESS: $total_hbad vector(s) reported on a comparison the suite cannot see:${total_hfailed}"
  echo "    Each is explained above. Fix the vector (or its row in harness-uncounted.txt);"
  echo "    reproduce without a CratonVM build via: bash regression-suite/harness-selfcheck.sh"
  total_fail=$((total_fail+total_hbad))
  total_failed="$total_failed$(printf '%s' "$total_hfailed" | sed 's/ / harness:/g')"
fi

echo "REGRESSION SUITE: $total_pass passed, $total_fail failed${total_failed:+ ( failed:$total_failed )}"
[ "$total_fail" -eq 0 ]
