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
#       When CRATONVM_ARGS names --jdk-only, every vector is ALSO given
#       --jdk-only-report writing to a PID-scoped directory, and the run ends
#       with a union census of what strict mode actually did (native-won versus
#       bytecode-won shadows, synthetic-native registrations, unenforced
#       shadows, compatibility classes, and how many reports SATURATED their
#       bounded sinks). Reported, never counted: it cannot change a verdict.
#       Suppressed if you pass your own --jdk-only-report.
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

# ROOT must FAIL LOUDLY rather than fall back. The old fallback was
# `|| echo C:/craton/CratonVM`, and on this case-insensitive filesystem that IS
# `C:/craton/cratonvm` — the main checkout, usually on `dev`. So any invocation
# where `git rev-parse` failed silently measured a DIFFERENT TREE and reported
# perfectly well-formed results about it.
#
# That is not hypothetical: running this script from a copy placed outside the
# repo (an attempt to make it immune to mid-run edits) sent `dirname
# "${BASH_SOURCE[0]}"` to a non-repo directory, took the fallback, and scheduled
# the main checkout's fixtures. It showed up as 19 `missing:` entries for
# fixtures that existed in the intended worktree.
# docs/known-issues/jdk-only/W8-D2-1-two-summary-lines-and-the-suite-denominator.md
ROOT="$(git -C "$(dirname "${BASH_SOURCE[0]}")" rev-parse --show-toplevel 2>/dev/null)"
if [ -z "$ROOT" ] || [ ! -d "$ROOT/regression-suite/src" ]; then
  echo "run.sh: cannot resolve the repository root from $(dirname "${BASH_SOURCE[0]}")." >&2
  echo "run.sh: run this script from inside its own worktree — do NOT copy it elsewhere," >&2
  echo "run.sh: because ROOT is derived from BASH_SOURCE and a copy relocates the run." >&2
  exit 3
fi
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

# The CLASS-PATH separator, for the one vector that needs a second entry
# (class_cp_extra below). Load-bearing and platform-dependent: `;` on Windows,
# `:` everywhere else. Both VMs are handed the same string, and both parse it
# with the host's convention — CratonVM's launcher and HotSpot agree here
# because the JDK's own rule is the host's, not the shell's. Derived from
# `uname` rather than from $JDK, because the suite is run from Git Bash on
# Windows (where java.exe wants `;`) and from a POSIX shell on the Linux build
# host (where it wants `:`), and $JDK looks the same in both.
case "$(uname -s 2>/dev/null)" in
  MINGW*|MSYS*|CYGWIN*|*NT-*) CPSEP=';' ;;
  *)                          CPSEP=':' ;;
esac

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
#
# RJdkOptionalShape is scheduled here and its REACH IS NARROWER THAN ITS NAME.
# Read this before quoting a green run of it as cover for C12-3.
#
# The vector's own "Mode independence" section says `http2.rs` "is registered in
# both arms", and that premise was MEASURED FALSE (lane E2, 2026-08-13): the
# nine Optional-minting sites in native-builtins/src/http2.rs only answer under
# `--synthetic-jdk`. In the default mode this list runs, a DIFFERENT
# implementation answers — net_phase_e.rs's re5_optional — and it is already
# correct. So the vector's httpmint() block, the only block written to reach
# C12-3, cannot exhibit C12-3 as scheduled.
#
# It is deliberately still scheduled, and the distinction is the point: this is
# NOT a gate that cannot fail. core(), prim(), stream(), version(), process()
# and misc() are real coverage of re5_optional and of the class library, and
# they go red if either regresses. What is untrue is only the CLAIM that a
# green RJdkOptionalShape says anything about http2.rs. A vector whose name
# overstates its reach is a labelling defect; unscheduling it to fix the label
# would delete working coverage to solve a documentation problem.
#
# Covering http2.rs needs a SECOND BINARY, not a flag: `--synthetic-jdk` is a
# runtime mode that a stock build refuses (exit 1); only a binary built with
# `--features synthetic-jdk` accepts it. That is why no arm is added here yet —
# an arm nobody on this box can execute would either skip (a gate that cannot
# fail, the exact defect this file's guards exist to catch) or fail every run
# for all eight lanes. The design constraints for landing one are in
# docs/known-issues/jdk-only/W8-E9-1-three-broken-oracles-and-the-suite-denominator.md.
# RJitMapTierDiff landed 2026-08-20 (lane H10) as H7-1 N1. It is in CORE and
# NOT in JDKONLY_CLASSES, for the same reason RJdkViews is: the thing it
# discriminates on only exists in --real-jdk. jit/src/lib.rs's four collection
# direct-helper triples are all `bridge` in
# scripts/baselines/jdk-only-kind-map-25-linux.tsv, so under --jdk-only
# direct_native_helper refuses every one of them at BIND time and strict mode
# has no second tier for these calls at all. CORE membership gets it BOTH arms
# anyway — SUITE_SET is "$CORE_CLASSES${JDK_ONLY:+ $JDKONLY_CLASSES}", so the
# strict run schedules it too and it PINS that refusal (every `moved=` must
# still read -1) — while a second registration in JDKONLY_CLASSES would only
# run the identical command twice.
#
# READ THIS BEFORE COMPARING TO A PUBLISHED BASELINE: it is the 105th vector.
# HANDOFF-20260820.md §1's `--jdk-only 104 / 104`, `SUITE=all 99 / 104` and
# `SUITE=core 63 / 64` all gain one to their DENOMINATOR. A run that reports
# 104/105 without naming which vector failed has not been read carefully.
#
# It is a two-tier fixture and that is the whole point: every shape is read at
# the first (interpreted) invocation and at the last, and the iteration at
# which the answer moved is PUBLISHED on every row, so a wrong answer that only
# appears after tier-up is arm-visible. Reading each shape once measures
# whichever tier happened to be right — the defect H4-1 O1 named as "a
# tier-dependent wrong answer no arm diffs for" and no arm in this suite could
# see. MEASURED on HotSpot 25.0.3+9 in the scheduled configuration: rc=0,
# PASS RJitMapTierDiff (75 checks), md5-identical over 3 runs and identical
# under -Xint. Falsified on purpose before landing (H10-1 §5).
#
# The list below is the UNION of both sides of the 2026-08-17 dev merge:
# this branch had 61 vectors, dev had 44, and RVarHandleAccess is dev's one
# addition this branch had never scheduled. Dropping either side would
# silently unschedule working coverage, which is the defect several of the
# guards further down exist to catch.
CORE_CLASSES="RCollections RStrings RNumbers RSerial RCrypto RExceptions RReflect ROptionalClassForName RPrivateLambdaOwner RLambdaDefaultOverload RJitGc RJitStringLayout RJitArrayTypecheck RJitArraycopyRefDeopt RJitMultiArrayClass RJitMapTierDiff RArrayStoreTiers RArrayStoreInterfaces RArraysMismatch RExecutorShutdown RBlockingQueue RChmKeySetView RChannelInterrupt RSocketChannelInterrupt RAtomicArray RDirectBufferElem RMapResizeGc RMapGcStress RForNameGcStress ROverlaySystemGcStress RFileTimes RNioNoFollow RSyncMethodJit RFieldSiteCache RMethodSiteCache RDataInputFastPull RCanAccessRules RChaCha20Cipher RLockedIdentityHash RCanAccessReceiver RForeignLayoutCollections RForeignLayoutJdkInterfaces RLoaderChurnDefine RClassUnloadSweep RClassUnloadSweepGen RPriorityQueueGc RTreeRangeGc RJdkViews RJdkFormatLocale RJdkStrictMath RJdkByteOrder RJdkIntrinsics RJdkIntrinsics2 RShutdownHooks RSimpleTimeZoneRaw RImmutableFactoryTypes RJdkStringCodePoints RFsSingleton RJdkOptionalShape RSimpleDateFormatZone RJdkIntrinsics3 RJdkBridge1 RSslNullSession RSslLiveSession RVarHandleAccess RStringBuilderContent RUnsafeArrayBase RLangPackages RSegmentBulkCopy RStreamToListCopy RFileChannelFastIo"

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
# RServiceLoaderDoubleSource landed 2026-08-13 (lane D1) and was registered
# NOWHERE until now: it compiled, produced a COVERAGE WARNING on every run, and
# would have been FATAL under STRICT_COVERAGE=1 — which this file's own
# documentation says CI should set. Its author left it unscheduled on purpose,
# because its only discriminating check needs one class-path entry the harness
# has to supply and it FAILS rather than skips when that entry is absent. That
# wiring is now here: class_cp_extra() puts the compiled module on -cp and
# class_args() hands both VMs the three properties that name it. Wiring first,
# registration second — the same ordering this file states for
# RPriorityQueueGc, and for the same reason: a vector registered before its
# input exists is red for a harness reason, which is noise, not coverage.
#
# It is in the JDK-ONLY list rather than CORE because that is where the defect
# was measured (four bc-java classes under `--jdk-only`, all four dying in
# EngineIdValidator before running a test), and because the record specifying
# this wiring nominates that list. It deliberately does NOT get a `--jdk-only`
# pin in class_cv_args: its discriminating assertion is
# ModuleLayer.boot().findModule(<a -cp-only module>), and the promotion it
# catches (populate_boot_layer_modules) is unconditional, so under `SUITE=all`
# with no CRATONVM_ARGS the vector still asks a real question about Compatible
# mode. Pinning the flag would hide whether the promotion happens there too.
#
# MEASURED on HotSpot 25.0.3+9 in exactly the scheduled configuration:
# rc=0, PASS RServiceLoaderDoubleSource (1266 checks), 6 lines out, 6 through
# extract(), all guards silent, md5-identical over 3 runs. It is predicted RED
# on CratonVM until the fix lands — that is the gate doing its job, not a bad
# registration.
# docs/known-issues/jdk-only/D1-R11-SERVICELOADER-DOUBLE-SOURCE-20260813.md
JDKONLY_CLASSES="RJdkHello RJdkStrict RJdkCollections RJdkLambdas RJdkHandles RJdkProxy RJdkReflect RJdkFieldModule RJdkRecords RJdkHidden RJdkModule RJdkServices RJdkAqs RJdkPhaser RJdkExecutors RJdkForkJoin RJdkNio RJdkNet RJdkProcess RJdkSecurity RJdkJmx RJdkJni RJdkFailure RJdkStampedStamps RJdkLookupIn RJdkDefineClass RJdkX509Intercept RJdkLogging RJdkSqlPackage RJdkEnvMap RJdkProxyIface RJdkFunctionCombinators RJdkForeign RJdkEnumerations RJdkAsyncChannel RJdkMapViews RServiceLoaderDoubleSource RJdkReflBox RJdkAwtHeadless RJdkWatchService"

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

# ---- G5: vectors whose reach is narrower than their schedule --------------
#
# One row per adjudication. Format, `|`-separated, `#` starts a comment line:
#
#     <Class>|<INERT|LIVE>|<family>|<mode>|<reason>
#
# INERT  a NAMED FAMILY of that vector's rows targets code that only runs in
#        <mode>, which is not the mode the vector is scheduled in. The family
#        executes and passes, for reasons unrelated to the defect it names.
# LIVE   the source NAMES <mode> — which is what the ADD scan keys on — and has
#        been adjudicated as genuinely discriminating where it is scheduled.
#
# THE UNIT IS THE FAMILY, NOT THE VECTOR, and that is the whole design. The row
# that motivated this guard is RJdkOptionalShape's: its httpmint() block is the
# only block written to reach C12-3, and lane E2 MEASURED that the nine
# Optional-minting sites in native-builtins/src/http2.rs answer only under
# --synthetic-jdk, while in the default mode this suite runs a different
# implementation (net_phase_e.rs's re5_optional) answers and is already correct.
# Its six other families are real default-mode coverage that goes red if the
# class library or re5_optional regresses. So the defect is a MISLABELLING of
# one family, not a vacuous fixture, and a guard that could not draw that line
# would demand the deletion of working coverage to fix a documentation problem.
#
# Both directions are loud (harness_guard_nondiscriminating): a listed vector
# whose source names the mode and has no row is an ADD error, and a row whose
# class, family or mode-mention has gone away — or whose mode the run is
# actually executing — is a STALE error. Clearing an entry is what makes the run
# green again, exactly as in harness-uncounted.txt.
#
# This is deliberately NOT a fifth term on the COUNTS: line. That line exists to
# stop four incommensurable populations being summed, and a hand-maintained
# fifth would undo it in the act of extending it; G5 findings are per-vector
# instrument flags and are counted with G1-G4 in the existing harness
# population. See W8-E9-1 §6 for the argument that was declined and W8-E15-1 for
# this one.
HARNESS_NONDISCRIMINATING="
RJdkOptionalShape|INERT|httpmint|--synthetic-jdk|the nine Optional-minting sites in native-builtins/src/http2.rs answer only under --synthetic-jdk (measured, lane E2 2026-08-13); in default mode net_phase_e.rs's re5_optional answers and is already correct. core/prim/stream/version/process/misc ARE live default-mode coverage.
RBlockingQueue|LIVE|-|--synthetic-jdk|its source names the synthetic-jdk Cargo feature only to record where the ORIGINAL defect lived (a synthetic <init> that never assigned takeLock). Every assertion is a queue CONTRACT asserted against whatever implementation answers, so the rows discriminate in the mode they are scheduled in.
RDirectBufferElem|LIVE|-|--synthetic-jdk|its own header states which natives its rows reach in the mode it is scheduled in (register_s2_bytebuffer, and the forced-native ByteBuffer methods) and states explicitly that they do NOT reach the native-io family that is synthetic-jdk-only. The mode is named to bound the claim, not to make it.
RJdkByteOrder|LIVE|-|--synthetic-jdk|the defect it was written against was synthetic-jdk-only, but every assertion is about the CONTENTS of the buffer and the IDENTITY of its backing array after order(), which is a mode-independent contract asked of whatever implementation answers.
"

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
prune_missing "$CORE_CLASSES";    # RJitMultiArrayClass landed 2026-08-17 with the multianewarray class-identity
# fix: the x64 lowering allocated every level of `new String[a][b]` with
# ClassId(0), so the compiled tier answered `[Ljava.lang.Object;` where the
# interpreter answered `[[Ljava.lang.String;`. It is a CORE vector because
# nothing about it is mode-specific, and it is a two-tier fixture because
# reading each shape once measures only the tier that was already right.
# MEASURED: red on the pre-fix binary (`CCE` at s16, `[Ljava.lang.Object;`
# at s00), green on the fixed one and green under --nojit on both.
# RJitArraycopyRefDeopt landed 2026-08-18 with the arraycopy deopt-snapshot fix:
# the x64 primitive-copy intrinsic pinned its five operands into scratch homes
# allocated from `next_spill_offset`, which the five pops had just rewound back
# over those operands' own frame slots, so the store of srcPos landed on dst's
# home — and the deopt snapshot, which names the ORIGINAL homes, then resumed a
# reference-array copy with srcPos where dst belonged. It is a CORE vector
# because a reference array is ALWAYS a bail here, i.e. this is ordinary correct
# code, not an error path. MEASURED: red on the pre-fix binary (NPE out of
# System.arraycopy), green after, green under --nojit on both, and byte-identical
# to HotSpot. Its witness() is layout-sensitive on purpose — see the class
# comment; a tidied-up first draft passed on the broken binary.
CORE_CLASSES="$PRUNED"
prune_missing "$JDKONLY_CLASSES"; JDKONLY_CLASSES="$PRUNED"

# `--jdk-only` in CRATONVM_ARGS implies the JDK-only corpus. The trailing space
# in the pattern keeps `--jdk-only-report <FILE>` from matching on its own.
case " ${CRATONVM_ARGS:-} " in
  *" --jdk-only "*) JDK_ONLY=1 ;;
esac

# ---- the strict census, folded into the arm (G84-1 N3) --------------------
#
# `--jdk-only-report` counts precisely what three P0 rows in
# docs/jdk-only-runtime-services.md argue about from source reading:
# `interpreter_shadow_unenforced`, the `native-shadows-bytecode` population
# split on its `outcome` field, `synthetic-native-registered`, and
# `compatibility_classes`. It costs ONE flag on a run that is already happening,
# and unlike the stub ratchet it needs no frozen baseline to be informative.
#
# THREE PROPERTIES THIS MUST NOT BREAK, and how each is kept:
#
#  * It must not change a verdict. The launcher writes the report from
#    `write_jdk_only_dumps`, which returns `()` and has no effect on the exit
#    code; a write failure is an `eprintln!` warning, not an error. The lines it
#    prints begin `[cratonvm] ` and `extract()` keeps only `^(PASS|CK) `, so
#    nothing reaches the cross-VM diff. Nothing here touches HotSpot's command
#    line — the oracle stays the plain reference run.
#  * It must not add a second FIXED shared path. `.guard-tmp` is fixed and two
#    concurrent runs already destroy each other's oracle files; this directory
#    is PID-scoped so it cannot repeat that. It is created once per invocation
#    and removed at the end, `rm -rf`, exactly like `.guard-tmp`.
#  * A missing report must be VISIBLE, not silent. The summary counts the
#    reports it expected against the ones that exist and prints the shortfall.
#    A census that is quietly absent is the failure mode this whole area exists
#    to remove.
#
# Skipped when the operator already passed their own `--jdk-only-report`: two
# copies of the flag would have the second silently win and write somewhere the
# summary below cannot see.
STRICT_REPORT=""
case " ${CRATONVM_ARGS:-} " in
  *" --jdk-only-report "*) : ;;
  *" --jdk-only "*)        STRICT_REPORT=1 ;;
esac
# PID-scoped, never a fixed shared name. See the note above.
REPORTDIR="$HERE/.jdk-only-reports.$$"
# How many per-vector reports we expect to find at the end. Incremented at the
# launch site rather than derived from $CLASSES, so a vector skipped for any
# reason cannot silently lower the denominator.
report_expected=0
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

# Identify the RUN, not just the result. Two run.sh processes sharing one
# redirect target produce two summaries in one file with no way to tell which
# tree, which revision or which schedule each belongs to — and because
# `git checkout` writes the working tree in index order, where
# regression-suite/run.sh (entry 63) precedes regression-suite/src/*.java
# (65+), a suite launched during a branch switch reads the NEW class lists
# against the OLD sources and reports every not-yet-written vector as
# `missing:`. Both happened on 2026-08-13 and cost a full audit to reconstruct
# from byte offsets. See docs/known-issues/jdk-only/
# W8-D2-1-two-summary-lines-and-the-suite-denominator.md.
echo "== RUN pid=$$ tree=$HERE rev=$(git -C "$HERE" rev-parse --short HEAD 2>/dev/null || echo '?') suite=${SUITE:-core} scheduled=$(printf '%s' "$CLASSES" | wc -w) missing=$(printf '%s' "$MISSING_CLASSES" | wc -w) =="
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

# The ENVIRONMENT-fault classifier. The `sig` grep in run_pass() can only find an
# assertion signature, and a VM that died before it reached the vector has none —
# a bad --java-home, a rejected command line and a missing main class all printed
# a bare `cratonvm rc=1`, indistinguishable from a real failure. See
# harness-vmfault.sh; its --selftest carries the negative controls.
. "$HERE/harness-vmfault.sh" || { echo "ERROR: cannot source $HERE/harness-vmfault.sh"; exit 3; }

# The census arithmetic. `sort -u` over whole JSON lines unions
# (triple, native_kind, outcome), not triples, so the same method was counted
# more than once; and the saturation grep asks for `truncated: true`, which
# cannot match the one sink that renders `null`. See harness-census.sh.
. "$HERE/harness-census.sh" || { echo "ERROR: cannot source $HERE/harness-census.sh"; exit 3; }

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

# class_args(), class_cp_extra() and class_cv_args() USED TO BE DEFINED HERE.
# They now have ONE definition, in harness-guard.sh, which this script sources
# at the top (search for `. "$HERE/harness-guard.sh"`) and which
# harness-selfcheck.sh sources too. Deleted 2026-08-13 (lane F9), applying
# W8-E30-1 NOM-3.
#
# The copies were behaviourally identical to the shared ones, not merely
# similar: W8-E30-1 §3.1's H1 guard compared them over 3 hooks x 95 classes x 4
# environments (HAVE_MODULE set/unset x CRATONVM_ARGS with/without
# --jdk-only — the four axes the three hooks actually branch on) = 1,140 rows,
# and got 1,140 identical answers. That is why the deletion is pure: the
# definitions that took over are the ones the matrix already proved equal, and
# H1 goes silent by itself with nothing left to compare rather than needing to
# be deleted alongside.
#
# Do not re-add a copy here to "override" the shared one for one vector. The
# later definition wins silently, which is exactly the drift that let this
# script and harness-selfcheck.sh disagree about RJdkModule until the hooks
# were unified. Add the arm to harness-guard.sh, where both callers see it.
#
# run.sh's own CPSEP is computed BEFORE the source and still wins;
# harness-guard.sh defaults CPSEP only when the caller left it unset, so there
# is exactly one value either way.
#
# MERGE 2026-08-16 — one arm arrived here from mainline AFTER the deletion, and
# it is NOT lost, it is RELOCATED. Mainline added `RClassUnloadSweepGen)
# printf '%s' "-XX:+UseGenerationalGC"` to its copy of class_cv_args while this
# branch was deleting that copy. Re-adding a copy to carry one arm is exactly
# what the paragraph above forbids, and it would not merely be untidy: the
# shared class_cv_args has grown RJdkForeign and RJdkSqlPackage arms that
# mainline's copy never had, so a copy here would DISAGREE with the shared
# definition and harness-guard.sh's H1 drift check would fire on every
# selfcheck. The arm belongs in harness-guard.sh's class_cv_args, beside
# RPriorityQueueGc and RTreeRangeGc, with mainline's reasoning:
#
#   The COLLECTOR is a variable this suite otherwise never moves: every other
#   vector runs on whatever the default happens to be, and that default changed
#   (Generational -> ZGC) on 2026-08-10 with nothing scheduled to notice.
#   `RClassUnloadSweepGen` is `RClassUnloadSweep`'s own probe run against the
#   generational young sweep, where the last surviving arm of
#   `TestDefaultInstanceManager`'s fourth recurrence lived. CratonVM-only by
#   construction: `$cvextra` never reaches the HotSpot oracle, which is correct
#   here — the claim is "a real JVM unloads this class", not "under a named
#   collector".
#
# UNTIL THAT ARM IS IN harness-guard.sh, RClassUnloadSweepGen RUNS UNDER THE
# DEFAULT COLLECTOR, which makes it a byte-for-byte re-run of
# RClassUnloadSweep — a scheduled vector that cannot fail for its own reason.
# That is the "gate that manufactures green" this file's guards exist to catch,
# so it is written down here rather than left to be noticed.
#
# The same mainline commit also added RClassUnloadSweepGen to CORE_CLASSES
# above; that half needed no relocation and is already in the merged list.

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
  # PID-SCOPED since 2026-08-21. This was `$HERE/.guard-tmp`, a FIXED path, and
  # the `rm -rf` below is the hazard: a second `run.sh` starting while a first is
  # mid-sweep deletes the first's oracle files underneath it. Both runs then diff
  # against nothing.
  #
  # It has now corrupted a measurement twice, and both times it was mistaken for
  # a VM defect first:
  #   * `H14-3` §5 — two concurrent sweeps starved the HotSpot oracle and moved
  #     `register_uri_natives` from 83/104 to **102/104**. Three arms discarded.
  #   * 2026-08-21, H0 — an orphaned background arms script overlapped a second
  #     `SUITE=all` and produced **3 passed, 204 failed**: every vector red AND a
  #     `harness:` entry for each. `204 = 102 vectors x 2`.
  #
  # THE SIGNATURE IS WORTH KNOWING: total redness that INCLUDES the harness guard
  # is an ENVIRONMENT failure, not a defect. No source change can fail `RJitGc`,
  # `RCrypto` and `RShutdownHooks` in the same run.
  #
  # `run.sh`'s own comment at the report-directory block already said a new
  # scratch path "must not add a second FIXED shared path … this directory is
  # PID-scoped so it cannot repeat that". The newer directory obeyed it; the
  # original never did. This closes that gap rather than adding a lock, because
  # concurrent sweeps are USEFUL — several workers can measure at once — and a
  # lock would serialise them for a reason that no longer exists.
  GUARDTMP="$HERE/.guard-tmp.$$"; rm -rf "$GUARDTMP"; mkdir -p "$GUARDTMP"
  # Leak-proof: the explicit `rm -rf` at the end of run_pass only runs on the
  # normal path, so an interrupted or timed-out sweep used to leave the directory
  # behind. With a PID suffix that would accumulate one per run.
  trap 'rm -rf "$GUARDTMP"' EXIT INT TERM
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
    cpx=$(class_cp_extra "$c")
    # An ARRAY, not a string, because $REPORTDIR is derived from the repository
    # path and may contain spaces — the flag lists above are expanded unquoted
    # on purpose and a path cannot join them. Empty array expands to nothing.
    cvreport=()
    if [ -n "$STRICT_REPORT" ]; then
      cvreport=(--jdk-only-report "$REPORTDIR/$c${REL:+-r$REL}.json")
      report_expected=$((report_expected+1))
    fi
    # $CRATONVM_ARGS, $extra and $cvextra are intentionally unquoted: all three
    # are flag lists, not single paths.
    cvout=$(CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 timeout "$TIMEOUT" "$CV" --java-home "$JDK" ${CRATONVM_ARGS:-} $cvextra $extra "${cvreport[@]}" -cp "$BUILD$cpx" "$c" 2>&1)
    cvrc=$?
    cvkey=$(printf '%s\n' "$cvout" | extract)
    # A failed assertion throws AssertionError → non-zero exit (handled by the rc
    # check), so we do NOT broad-grep for "Exception"/"Error" — tests intentionally
    # throw-and-catch, and CratonVM traces those, which would false-fail. We only
    # flag hard VM crashes that may not set a non-zero rc.
    state=PASS; why=""
    if [ "$cvrc" -ne 0 ]; then
      state=FAIL; why="cratonvm rc=$cvrc"
      # ENVIRONMENT faults FIRST. A VM that never reached the vector has no
      # assertion signature for the alternation below to find, so every one of
      # them used to print the bare `cratonvm rc=$cvrc` above — the same line a
      # genuine assertion failure prints. The vector is still red; the `why` now
      # says the harness is broken instead of implying the VM answered wrongly.
      if envwhy=$(vm_fault_class "$cvrc" "$cvout"); then
        why="$envwhy"
        # Accumulated for ONE explanation at the bottom. A broken --java-home
        # fails all 105 vectors, and the fix printed 105 times is noise.
        ENV_FAULT_N=$((ENV_FAULT_N+1)); ENV_FAULT_LAST="$envwhy"
      else
      sig=$(printf '%s\n' "$cvout" | grep -aiE 'AssertionError|NoSuchMethod|linkage error|panic|SEGV|fatal' | grep -avE '^\s*at ' | tail -1 | sed 's/\x1b\[[0-9;]*m//g' | head -c 90)
      # ---------------------------------------------------------------------
      # A LAUNCH/CONFIG FAILURE USED TO RENDER EXACTLY LIKE AN ASSERTION
      # FAILURE (2026-08-21).
      #
      # The pattern list above is a list of things somebody had already seen.
      # Anything else produced an EMPTY `sig`, so `why` stayed the bare
      # `cratonvm rc=1` — and a vector that never started looked identical to a
      # vector that ran and failed an assertion. `H24-2` hit this and came
      # within one step of reporting three of this week's closures reverted;
      # `H24-2`'s own A/B is reproducible today (a `JDK` exported in the MSYS
      # POSIX spelling reddens the vector, the `cygpath -m` spelling passes).
      #
      # Rather than append one more regex per failure mode — a list that is
      # incomplete by construction and was already wrong once — fall through to
      # the VM's OWN first line of output. Two extra steps, in order:
      #
      #   1. clap's argument errors, which are `^error: ` / `^Usage: ` /
      #      `For more information, try '--help'`;
      #   2. anything at all that is not tracing, a stack frame, VM chatter or
      #      blank — tagged `unclassified:` so it is obvious the harness did not
      #      recognise it and the pattern list may deserve a new entry.
      #
      # A vector that dies before printing anything now says `no output` rather
      # than implying an assertion fired.
      if [ -z "$sig" ]; then
        sig=$(printf '%s\n' "$cvout" | sed 's/\x1b\[[0-9;]*m//g' \
              | grep -aE '^error: |^Usage: |For more information, try' | head -1 | head -c 90)
      fi
      if [ -z "$sig" ]; then
        first=$(printf '%s\n' "$cvout" | sed 's/\x1b\[[0-9;]*m//g' \
                | grep -avE '^\s*at |WARN|^\[cratonvm\]|^\s*$' | head -1 | head -c 70)
        if [ -n "$first" ]; then sig="unclassified: $first"; else sig="no output"; fi
      fi
      [ -n "$sig" ] && why="rc=$cvrc: $sig"
      fi
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
    HARNESS_GUARD_MSGS=""; guarded=0; guarded_oracle=0
    if [ -x "$HS" ]; then
      timeout "$TIMEOUT" "$HS" $extra -cp "$BUILD$cpx" "$c" > "$GUARDTMP/hs.raw" 2>&1
      hsrc=$?
      hskey=$(extract < "$GUARDTMP/hs.raw")
      harness_guard_oracle "$c" "$GUARDTMP/hs.raw" "$hsrc" || { guarded=1; guarded_oracle=1; }
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
      printf '%s\n' "$HARNESS_GUARD_MSGS"
      # COUNTED only when it is an INDEPENDENT finding.
      #   G1/G4 (oracle-side) always are: a sick or silent oracle invalidates
      #   the ground truth whatever CratonVM did.
      #   G2/G3 read CratonVM's surviving output, so on a vector that ALREADY
      #   FAILED they merely restate the failure — a vector that throws before
      #   its banner leaves an empty cv.key BY CONSTRUCTION, and G2+G3 then fire
      #   for it every single time. On 2026-08-13 all 9 harness flags in a
      #   SUITE=all run sat on vectors already counted red: 9 duplicate points
      #   in "21 failed", 0 independent findings. The blindness G2/G3 exist to
      #   catch (W7-60) is a vector that PASSES with nothing observable, so ask
      #   them about a PASS. The messages print above either way, so a red
      #   vector that is also uncounted is still visible — it just does not
      #   inflate the number.
      #
      # SAFETY PROPERTY, verified rather than assumed: this can only ever
      # SUBTRACT a point from a vector that is already contributing one via
      # $fail, so `total_fail` cannot reach 0 by way of this branch and a run
      # that should be red cannot become green. The suppressed direction is
      # strictly "stop double-counting"; it is never "stop failing".
      if [ "$state" = PASS ] || [ "$guarded_oracle" -ne 0 ]; then
        hbad=$((hbad+1)); hfailed="$hfailed $c"
      fi
    fi
  done
  rm -rf "$GUARDTMP"
  return 0
}

total_pass=0; total_fail=0; total_failed=""; ran=0; skipped=""
total_hbad=0; total_hfailed=""
# ENVIRONMENT faults: vectors whose VM never started. Counted here only so the
# summary can explain them ONCE; they are already red via $fail and this must
# never add a second point for the same vector.
ENV_FAULT_N=0; ENV_FAULT_LAST=""

# Created once per invocation, not once per pass: with RELEASES= set, run_pass
# runs several times and every pass's reports belong to the one summary at the
# bottom. Cleaned there. A mkdir failure DISABLES the census rather than failing
# the run — the report is an instrument bolted onto the verdict, and an
# instrument must never be the thing that turns a run red.
if [ -n "$STRICT_REPORT" ]; then
  rm -rf "$REPORTDIR"
  if ! mkdir -p "$REPORTDIR" 2>/dev/null; then
    echo "  NOTE: cannot create $REPORTDIR — the --jdk-only census is SKIPPED for this run"
    STRICT_REPORT=""
  fi
fi

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

# Snapshot the ONLY failure population that shares a denominator with
# $total_pass — scheduled vectors that ran and lost — before list errors,
# coverage errors and harness flags are folded into $total_fail below.
total_vecfail=$total_fail
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

# ---- G5, the reach ratchet ------------------------------------------------
#
# Run ONCE per invocation, not once per vector: it compares the class LISTS
# against the vector SOURCES and against the mode this run is executing, and
# none of those change between passes. Uses LISTED_CLASSES (both lists, captured
# before prune_missing) so its answer does not depend on SUITE.
#
# Counted into $total_hbad — the existing harness-flag population — so the
# COUNTS: line stays a four-term decomposition and still closes. A G5 finding is
# a per-vector flag on a scheduled vector, which is precisely what that term
# already says it holds; it is not another vector and not a list error.
HARNESS_GUARD_MSGS=""
if ! harness_guard_nondiscriminating "$HERE/src" "$LISTED_CLASSES"; then
  printf '%s\n' "$HARNESS_GUARD_MSGS"
  total_hbad=$((total_hbad+HARNESS_G5_BAD))
  total_hfailed="$total_hfailed$HARNESS_G5_CLASSES"
fi

# ---- the environment's own verdict ----------------------------------------
#
# Printed BEFORE the harness verdict and the totals, because when this fires the
# numbers below it describe a run that never happened. NOT added to $total_fail:
# every vector it names is already counted red by $fail, and a second point
# would be the double-count the G2/G3 note above exists to avoid.
if [ "$ENV_FAULT_N" -gt 0 ]; then
  echo "  ENVIRONMENT: $ENV_FAULT_N vector(s) had no VM to answer them — the run below is not a"
  echo "  measurement of CratonVM. Last seen:"
  echo "    $ENV_FAULT_LAST"
  vm_fault_hint "$ENV_FAULT_LAST"
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
  echo "    Each is explained above. Fix the vector, or its row in the table the guard names:"
  echo "    G3 -> regression-suite/harness-uncounted.txt, G5 -> HARNESS_NONDISCRIMINATING in this file."
  echo "    G1-G4 reproduce without a CratonVM build via: bash regression-suite/harness-selfcheck.sh"
  total_fail=$((total_fail+total_hbad))
  total_failed="$total_failed$(printf '%s' "$total_hfailed" | sed 's/ / harness:/g')"
fi

# $total_fail is the EXIT-STATUS accumulator and sums four incommensurable
# populations: scheduled vectors that lost; registrations with no source and
# sources with no registration (neither ever ran); and one point per vector
# whose instrument-blindness guard fired (which is a FLAG ON a vector, usually
# one already counted red above — not another vector). Only the first shares a
# denominator with $total_pass, so "$total_pass passed, $total_fail failed" is
# not a pair and must never be quoted as one. Print the decomposition so the
# reader does not have to read this script to interpret the line.
echo "REGRESSION SUITE: $total_pass passed, $total_fail failed${total_failed:+ ( failed:$total_failed )}"
echo "  COUNTS: $total_pass of $((total_pass+total_vecfail)) SCHEDULED vectors passed; $total_vecfail scheduled vectors failed; $((total_fail-total_vecfail-total_hbad)) list/coverage errors (never scheduled); $total_hbad harness-blindness flags (per-vector flags, not extra vectors)."

# ---- the --jdk-only census, unioned over the vectors that just ran --------
#
# Reported, never counted: not one line below touches $total_fail. This is a
# MEASUREMENT of the mode the arm ran in, and the three P0 rows it feeds want a
# trend, not a gate — there is no frozen baseline here and deliberately so
# (G84-1 N3). A census that could turn a run red would acquire a baseline, and a
# baseline is exactly what makes the stub ratchet unable to tell a regression
# from an improvement.
#
# Every row of `violations[]` is ONE LINE of compact JSON (types::error's
# to_json has no serde and no pretty-printer), which is why grep/sort is enough
# and no jq is required. Rows are byte-identical across vectors for the same
# fact — `summary` is a pure function of the other fields — so `sort -u` is a
# real UNION and not an approximation. Counter keys are the pretty-printed ones
# with a space after the colon; violation rows have none, so the two can never
# be confused by these patterns.
if [ -n "$STRICT_REPORT" ]; then
  echo "---------------------------------------------"
  jr_found=$(ls "$REPORTDIR"/*.json 2>/dev/null | wc -l | tr -d ' ')
  jr_rows=$(grep -h '"kind":"native-shadows-bytecode"' "$REPORTDIR"/*.json 2>/dev/null \
              | sed 's/^[[:space:]]*//; s/,$//' | sort -u)
  # The whole-line counts, KEPT because every record written before 2026-08-21
  # quotes them and a reader has to be able to reconcile the two.
  jr_native_lines=$(printf '%s\n' "$jr_rows" | grep -c '"outcome":"native-won"')
  jr_bytecode_lines=$(printf '%s\n' "$jr_rows" | grep -c '"outcome":"bytecode-won"')
  # The counts by TRIPLE, which is the unit docs/known-issues/jdk-only/ quotes.
  jr_sum=$(printf '%s\n' "$jr_rows" | census_shadow_summary)
  jr_native=$(printf '%s' "$jr_sum" | sed -n 's/.* native=\([0-9]*\).*/\1/p')
  jr_bytecode=$(printf '%s' "$jr_sum" | sed -n 's/.* bytecode=\([0-9]*\).*/\1/p')
  jr_both=$(printf '%s' "$jr_sum" | sed -n 's/.* both=\([0-9]*\).*/\1/p')
  jr_bconly=$(printf '%s' "$jr_sum" | sed -n 's/.* bytecode_only=\([0-9]*\).*/\1/p')
  jr_stubs=$(grep -h '"kind":"synthetic-native-registered"' "$REPORTDIR"/*.json 2>/dev/null \
               | sed 's/^[[:space:]]*//; s/,$//' | sort -u | grep -c '"kind"')
  jr_unenf=$(grep -h '"interpreter_shadow_unenforced": ' "$REPORTDIR"/*.json 2>/dev/null \
               | sed 's/[^0-9]//g' | awk '{s+=$1} END {print s+0}')
  jr_compat=$(grep -h '"compatibility_classes": ' "$REPORTDIR"/*.json 2>/dev/null \
                | sed 's/[^0-9]//g' | awk '{s+=$1} END {print s+0}')
  # Whether every figure above is a total, a FLOOR, or UNKNOWN is
  # census_saturation's job — see harness-census.sh for why `null` is a third
  # answer and not a quiet `false`.
  echo "JDK-ONLY CENSUS ($jr_found of $report_expected per-vector reports written):"
  echo "  native-shadows-bytecode, UNION over vectors, counted by TRIPLE:"
  echo "    $jr_native native-won (the defect)   ·   $jr_bytecode bytecode-won"
  echo "    of the $jr_bytecode bytecode-won triples, $jr_both ALSO ran the native in another"
  echo "    vector; $jr_bconly were bytecode-won and NEVER native — that is 'the contract"
  echo "    working', and it is the only one of these numbers that means it."
  echo "    (whole-line rows, the pre-2026-08-21 figures: $jr_native_lines / $jr_bytecode_lines. A whole-line"
  echo "     sort -u unions (triple, native_kind, outcome), so a triple that dispatched"
  echo "     two ways is counted twice. harness-census.sh has the measurement.)"
  echo "  synthetic-native-registered, UNION: $jr_stubs   ·   interpreter_shadow_unenforced, SUM: $jr_unenf   ·   compatibility_classes, SUM: $jr_compat"
  census_saturation "$REPORTDIR" || true
  if [ "$jr_found" -lt "$report_expected" ]; then
    echo "  NOTE: $((report_expected-jr_found)) vector(s) produced no report (a crash before the exit hook, or a write failure)."
    echo "    Their shadows are missing from the union above. This does NOT affect any vector's verdict."
  fi
  rm -rf "$REPORTDIR"
fi

[ "$total_fail" -eq 0 ]
