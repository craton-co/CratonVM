#!/bin/bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
#
# Criterion 6 of docs/feature-designs/jdk-only-mode.md section 11: "the strict
# corpus is green". This is the gate for it.
#
# It runs every probe in the strict corpus THREE times -- real HotSpot,
# `cratonvm --real-jdk`, `cratonvm --jdk-only` -- against the same JDK image and
# the same class files, then diffs the two CratonVM transcripts against the
# HotSpot one. A divergence present in BOTH CratonVM modes is a compatibility
# defect and is reported as such; a divergence present only under `--jdk-only`
# is a strict-mode defect. Telling those apart is the entire point of running
# three arms instead of two, and it is what the lane's history says gets skipped.
#
# It also fails on a non-zero exit status even when the transcript matches,
# because a `timeout` kill prints a truncated transcript that reads exactly like
# a clean short run -- the specific way the first two strict census runs lied.
#
# IT IS A RATCHET, NOT A PASS/FAIL ON GREEN. The corpus is not green today --
# four defects are filed against it -- so a gate that demanded zero divergence
# would be red on every build and off within a week, which is the decorative
# guard this lane exists to avoid. Instead the KNOWN divergences are committed
# to a baseline and the gate fails when the set GROWS. A divergence that
# disappears never fails; it prints "re-freeze" and passes, so a fix is never
# blocked by the gate that measured it.
#
# What that buys and what it costs, stated plainly: a NEW divergent section
# fails the build, which is the whole point. A known-divergent section whose
# wrong VALUE changes to a different wrong value does NOT fail, because the
# baseline records section identity rather than content. Freezing content would
# fail on `vthreads`, whose divergence is intermittent by nature (the
# ConcurrentHashMap.newKeySet defect is a race), and a gate that flakes is a
# gate that gets disabled.
#
# Usage:
#   JAVA_HOME=/path/to/real/jdk25 \
#   CV=target/release/cratonvm \
#   [OUT=target/jdk-only-strict-probes] \
#   [TIMEOUT=300] \
#   [PROBE_LIST="JdkOnlyCensusLoadProbe ..."] \
#   scripts/jdk-only-strict-probes.sh [--update-baseline --note "why"]
#
# The baseline is keyed `<jdk-feature>-<os>` because the transcripts are a
# property of the image and the platform, exactly like the bridge ratchet's:
#   scripts/baselines/jdk-only-strict-corpus-25-linux.txt
# It is NEVER written by hand -- `--update-baseline` regenerates it from a real
# run and refuses without a `--note` saying why the set moved.
#
# Exit codes:
#   0  every arm completed and the divergent set is within the baseline
#   2  a prerequisite is missing; or a FIXTURE did not build and the resulting
#      degradation was not declared; or there is no baseline for this
#      <feature>-<os> and the gate refuses to adjudicate (never a pass)
#   3  a probe failed to compile
#   4  an arm did not complete (non-zero exit, timeout, or crash)
#   5  the ratchet fired: a section diverged that the baseline does not carry
#
# 4 is NEVER baselined. A hang or a crash fails the build whatever the baseline
# says -- there is no such thing as a known-acceptable truncated transcript,
# and that is the specific way the first two strict census runs lied.
#
# 4 and 5 are separate on purpose: a truncated run and a wrong answer need
# different triage, and collapsing them into "failed" is how a hang gets filed
# as a diff.
#
# A FIXTURE THAT DID NOT BUILD IS NOT AN AGREEMENT (2026-08-12).
# ------------------------------------------------------------
# Until this date the two fixture builders below -- the agent jar and the JNI
# shared object -- printed a WARNING when they failed and let the run continue,
# and the comments here asserted that this was safe because "the arms still
# agree and the gate stays honest". THAT ARGUMENT IS WRONG, and it is wrong in
# the exact way this repo has already named twice.
#
# This gate measures AGREEMENT between three arms. When a fixture is missing,
# JdkOnlyPlatformProbe prints `agent=absent` / `jni ... lib=absent` in ALL
# THREE arms, because none of them was given the fixture. The three arms then
# agree -- not because they behaved the same, but because none of them
# executed the code under test. Agreement between three instruments that all
# measured nothing is the ABSENCE OF EVIDENCE, not evidence of sameness. The
# whole agent section and the whole JNI section switch themselves off, the
# ratchet has nothing left to fire on, and the script prints PASS.
#
# That is G4 of regression-suite/harness-guard.sh -- "the run that supplies
# ground truth did not itself succeed, so the 'expected' side of the diff is an
# artefact of its failure" -- arriving one level up, and it is the shape
# docs/known-issues/jdk-only/W7-60-harness-extract-blindness.md §6.6 rejects in
# so many words: a warning inside a green build is how the previous version of
# this defect survived long enough to be measured. It is worse than an ordinary
# vacuous check because it is SELF-DISABLING: the harness silently reduces its
# own coverage and still reports success.
#
# So: a fixture that did not build makes the run REFUSE (exit 2), reusing the
# refusal the self-test and the missing-baseline paths already use. Degradation
# is still allowed -- some hosts genuinely have no C compiler -- but it must be
# DECLARED, exactly as regression-suite/harness-uncounted.txt makes an uncounted
# vector declared rather than merely tolerated:
#
#   ALLOW_DEGRADED_FIXTURES=1                     allow every degradation
#   ALLOW_DEGRADED_FIXTURES="jni-lib-no-cc"       allow exactly that one
#
# Prefer the second spelling. `=1` also silences a fixture that used to build
# and has just started failing, which is the regression you wanted to hear
# about. An UNDECLARED degradation refuses; a declared one runs and says out
# loud, in the verdict, which sections it did not cover. A permanently-red job
# is a job nobody reads, so if a matrix leg genuinely cannot build a fixture,
# name that fixture in the leg's ALLOW_DEGRADED_FIXTURES with a reason in the
# workflow comment -- do not leave the leg red.
#
# WHY THIS SCRIPT IN PARTICULAR. `apps/probes/` (formerly the repo-root
# `probes/`) is scheduled by nothing else: `grep -c 'probes/'
# regression-suite/run.sh` is 0, so no SUITE= value of the regression suite
# runs a single probe. This script and scripts/jdk-only-census.sh are the only
# scheduled consumers of the probe corpus, and this script is the only one
# that runs a HotSpot control. When its JNI and agent sections switch
# themselves off, the JNI boundary and instrumentation under --jdk-only are
# covered by NOTHING, anywhere, and no suite run at any SUITE= value would
# notice.
set -u

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${OUT:-$ROOT/target/jdk-only-strict-probes}"
TIMEOUT="${TIMEOUT:-300}"

UPDATE=0
NOTE=""
while [ $# -gt 0 ]; do
  case "$1" in
    --update-baseline) UPDATE=1; shift ;;
    --note) NOTE="${2:-}"; shift 2 ;;
    --note=*) NOTE="${1#--note=}"; shift ;;
    *) echo "ERROR: unknown argument: $1" >&2; exit 2 ;;
  esac
done
if [ "$UPDATE" -eq 1 ] && [ -z "$NOTE" ]; then
  echo "ERROR: --update-baseline requires --note \"why the divergent set moved\"." >&2
  echo "       A baseline bump with no reason is how a regression gets frozen in." >&2
  exit 2
fi

# ---------------------------------------------------------------- prerequisites

if [ -z "${JAVA_HOME:-}" ]; then
  echo "ERROR: JAVA_HOME must point at a REAL JDK image (21+; 25 for the"
  echo "       virtual-thread and agent sections to mean anything)."
  exit 2
fi

case "$(uname -s 2>/dev/null || echo unknown)" in
  MINGW*|MSYS*|CYGWIN*) EXE=.exe ;;
  *) EXE= ;;
esac

JAVA="$JAVA_HOME/bin/java$EXE"
JAVAC="$JAVA_HOME/bin/javac$EXE"
JAR="$JAVA_HOME/bin/jar$EXE"
for tool in "$JAVA" "$JAVAC" "$JAR"; do
  if [ ! -x "$tool" ]; then
    echo "ERROR: $tool is not executable. JAVA_HOME=$JAVA_HOME is not a JDK image."
    exit 2
  fi
done

CV="${CV:-$ROOT/target/release/cratonvm$EXE}"
if [ ! -x "$CV" ]; then
  echo "ERROR: CV=$CV is not executable. Build it, or point CV at the binary."
  exit 2
fi

mkdir -p "$OUT/classes" "$OUT/logs"

# ------------------------------------------------------- the ratchet machinery

# A probe prints exactly one line per section and every line begins with its
# section name, so a divergence is identified by (probe, arm, section). That is
# the unit the baseline records.
#
# `SECTION-FAILED collections: java.lang.NoClassDefFoundError: ...` is keyed
# `SECTION-FAILED:collections` rather than `SECTION-FAILED`, so five different
# sections blowing up are five entries and not one.
divergent_keys() {
  # stdin: a unified diff. stdout: one section key per line, sorted, unique.
  grep -E '^[-+]' \
    | grep -vE '^(---|\+\+\+)' \
    | sed -E 's/^[-+]//' \
    | awk '
        /^SECTION-FAILED /{ k=$2; sub(/:$/,"",k); print "SECTION-FAILED:" k; next }
        NF                { print $1 }
      ' \
    | sort -u
}

# Which of the fixtures that failed to build were NOT declared by the operator.
# Exactly one copy of this decision exists, and both the self-test below and the
# verdict at the bottom call it, so the rule that is tested and the rule that is
# enforced cannot drift apart -- the same reason harness-guard.sh defines
# extract() exactly once.
#
# $1 = degraded tokens (space-separated), $2 = ALLOW_DEGRADED_FIXTURES.
# stdout: the undeclared tokens, space-prefixed. Empty output means "declared".
undeclared_degradations() {
  ud_allowed=" $(printf '%s' "$2" | tr ',' ' ') "
  ud_out=""
  for ud_f in $1; do
    case "$ud_allowed" in
      *" 1 "*|*" all "*) continue ;;
      *" $ud_f "*) continue ;;
    esac
    ud_out="$ud_out $ud_f"
  done
  printf '%s' "$ud_out"
}

# Detect the JDK feature version the same way scripts/jdk-only-census.sh does,
# because the baselines are keyed by it and two different keys must never be
# derived two different ways.
detect_jdk_feature() {
  home="$1"; raw=""
  if [ -f "$home/release" ]; then
    raw="$(sed -n 's/^JAVA_VERSION=//p' "$home/release" 2>/dev/null | head -n 1 | tr -d '"' | tr -d '\r')"
  fi
  if [ -z "$raw" ] && [ -x "$home/bin/java$EXE" ]; then
    raw="$("$home/bin/java$EXE" -version 2>&1 | sed -n 's/.*version "\([^"]*\)".*/\1/p' | head -n 1 | tr -d '\r')"
  fi
  [ -n "$raw" ] || return 1
  case "$raw" in 1.*) raw="${raw#1.}" ;; esac
  feature="${raw%%[!0-9]*}"
  [ -n "$feature" ] || return 1
  printf '%s\n' "$feature"
}

case "$(uname -s 2>/dev/null || echo unknown)" in
  Linux) OSKEY=linux ;;
  Darwin) OSKEY=macos ;;
  MINGW*|MSYS*|CYGWIN*) OSKEY=windows ;;
  *) OSKEY=unknown ;;
esac
FEATURE="${JDK_FEATURE:-}"
if [ -z "$FEATURE" ]; then
  FEATURE="$(detect_jdk_feature "$JAVA_HOME")" || {
    echo "ERROR: could not read JAVA_VERSION from $JAVA_HOME/release, and"
    echo "       $JAVA_HOME/bin/java -version did not name one. The baseline is"
    echo "       keyed by JDK feature version, so this cannot be adjudicated."
    exit 2
  }
fi
BASELINE="$ROOT/scripts/baselines/jdk-only-strict-corpus-$FEATURE-$OSKEY.txt"
echo "baseline key: $FEATURE-$OSKEY"

# --------------------------------------------------- hermetic self-test first
#
# The gate's own logic, exercised on fabricated transcripts, before it is
# pointed at a VM. It asserts BOTH directions: an injected new section is
# flagged, and a section already in the baseline is not. A ratchet that has
# never been shown to fire is indistinguishable from one that cannot.
selftest() {
  st="$OUT/selftest"; rm -rf "$st"; mkdir -p "$st"
  printf 'alpha ok=1\nbeta ok=1\ngamma ok=1\n'          > "$st/hotspot"
  printf 'alpha ok=1\nbeta ok=2\nSECTION-FAILED gamma: boom\n' > "$st/actual"
  observed="$(diff -u "$st/hotspot" "$st/actual" | divergent_keys)"
  want="$(printf 'SECTION-FAILED:gamma\nbeta\ngamma\n' | sort -u)"
  if [ "$observed" != "$want" ]; then
    echo "ERROR: self-test failed -- key extraction is wrong."
    echo "  expected: $(echo "$want" | tr '\n' ' ')"
    echo "  observed: $(echo "$observed" | tr '\n' ' ')"
    return 1
  fi
  # Baseline carrying `beta` must leave `beta` unflagged and still flag the rest.
  printf 'p/strict/beta\n' > "$st/base"
  newk="$(printf 'p/strict/%s\n' $observed | sort -u | comm -23 - "$st/base")"
  case "$newk" in
    *"p/strict/beta"*) echo "ERROR: self-test failed -- a baselined key was flagged as new."; return 1 ;;
  esac
  case "$newk" in
    *"p/strict/gamma"*) : ;;
    *) echo "ERROR: self-test failed -- a NEW key was not flagged. The ratchet cannot fire."; return 1 ;;
  esac

  # The degradation guard, both directions, on the same principle: a guard that
  # has never been shown to fire is indistinguishable from one that cannot --
  # and this guard exists precisely because the previous version of it silently
  # could not.
  #   1. an undeclared fixture is reported (the guard fires)
  #   2. a declared one is not (the escape hatch works, so nobody deletes it)
  #   3. a blanket `1` covers everything
  #   4. a PARTIAL declaration still reports the rest -- the case that makes
  #      the list spelling worth having over `=1`
  if [ -z "$(undeclared_degradations 'jni-lib' '0')" ]; then
    echo "ERROR: self-test failed -- an UNDECLARED degraded fixture was not"
    echo "       reported. The degradation guard cannot fire."
    return 1
  fi
  if [ -n "$(undeclared_degradations 'jni-lib' 'jni-lib')" ]; then
    echo "ERROR: self-test failed -- a DECLARED degraded fixture was still"
    echo "       reported. ALLOW_DEGRADED_FIXTURES does not work."
    return 1
  fi
  if [ -n "$(undeclared_degradations 'jni-lib agent-jar' '1')" ]; then
    echo "ERROR: self-test failed -- ALLOW_DEGRADED_FIXTURES=1 did not allow all."
    return 1
  fi
  if [ "$(undeclared_degradations 'jni-lib agent-jar' 'jni-lib')" != " agent-jar" ]; then
    echo "ERROR: self-test failed -- a PARTIAL declaration did not leave the"
    echo "       undeclared fixture reported."
    return 1
  fi
  return 0
}
if ! selftest; then
  echo "RESULT: REFUSED -- the gate cannot adjudicate because its own logic is broken."
  exit 2
fi
echo "self-test: the ratchet fires on a new section and not on a baselined one"
echo "self-test: an undeclared degraded fixture refuses; a declared one does not"

# ------------------------------------------------------------------ the corpus

# JdkOnlyIcHotProbe is deliberately absent: it exists to drive the refusal
# counters under a JIT-hot workload, and its transcript is a throughput
# artefact, not a value to diff. scripts/jdk-only-measure-refusals-and-overlays.sh
# is where it belongs.
# The default list is three probes. `apps/probes/` holds 107 files, and until
# 2026-09-08 nothing scheduled the other 104 -- not because they are worthless
# but because there was no way to add one to a SINGLE platform.
#
# The ratchet is a set difference against a baseline keyed <feature>-<os>.
# Appending a probe HERE puts its divergent sections into `observed` on every
# leg at once, including legs whose baseline was frozen without them, where
# every such section reads as NEW and the gate fails. So promoting a probe
# measured on one platform used to require measuring it on all of them first,
# and the corpus stayed frozen at three.
#
# The promoted set is therefore keyed exactly like the baseline it is scored
# against. Each non-comment line of
#     scripts/baselines/jdk-only-strict-corpus-<feature>-<os>.probes
# names one probe to run on THAT key only; a key with no such file runs the
# three below and nothing else, unchanged. Promote a probe by measuring it on
# a key and adding it to that key's file -- never by editing this list.
#
# An explicit PROBE_LIST= in the environment still wins outright, so the
# single-probe invocations used for triage are unaffected by either.
PROBE_LIST_DEFAULT="JdkOnlyCensusLoadProbe JdkOnlyBreadthProbe JdkOnlyPlatformProbe"
PROMOTED="$ROOT/scripts/baselines/jdk-only-strict-corpus-$FEATURE-$OSKEY.probes"
if [ -n "${PROBE_LIST+set}" ]; then
  echo "probe list:   PROBE_LIST= from the environment (promoted file not read)"
else
  PROBE_LIST="$PROBE_LIST_DEFAULT"
  if [ -f "$PROMOTED" ]; then
    # `sort -u` so a duplicated line, or one that repeats a default probe,
    # cannot schedule the same probe twice: the harness writes its logs to
    # $OUT/logs/<probe>.* and a second pass would overwrite the first, leaving
    # a transcript whose name no longer says which run produced it.
    extra="$(grep -vE '^[[:space:]]*(#|$)' "$PROMOTED" | tr -d '\r' | tr -s '[:space:]' '\n' \
             | grep -vxF -e JdkOnlyCensusLoadProbe -e JdkOnlyBreadthProbe -e JdkOnlyPlatformProbe \
             | grep -v '^$' | sort -u | tr '\n' ' ')"
    PROBE_LIST="$PROBE_LIST $extra"
    echo "promoted:     $(printf '%s' "$extra" | wc -w) probe(s) from $(basename "$PROMOTED")"
  else
    echo "promoted:     none -- no $(basename "$PROMOTED")"
  fi
fi

SRCS=""
for p in $PROBE_LIST; do
  if [ ! -f "$ROOT/apps/probes/$p.java" ]; then
    echo "ERROR: apps/probes/$p.java does not exist."
    exit 3
  fi
  SRCS="$SRCS $ROOT/apps/probes/$p.java"
done
# The agent is compiled with the probes so JdkOnlyPlatformProbe's reflective
# lookup has something to find; it is never listed as a probe itself.
if [ -f "$ROOT/apps/probes/JdkOnlyProbeAgent.java" ]; then
  SRCS="$SRCS $ROOT/apps/probes/JdkOnlyProbeAgent.java"
fi

echo "== compiling the strict corpus =="
if ! "$JAVAC" -d "$OUT/classes" $SRCS 2>&1; then
  echo "ERROR: the probe corpus did not compile."
  exit 3
fi

# Set by any fixture that failed to build. A missing fixture makes its section
# report `absent` in EVERY arm, so the arms AGREE and the agreement ratchet sees
# no divergence -- the section is switched off and the gate would still say
# PASS. That is the G4 defect of W7-60-harness-extract-blindness.md: a control
# that produced no evidence being scored as ground truth. Declared degradation
# is fine; silent degradation is not. Adjudicated once, at the verdict.
#
# Each token names a fixture, not a section, because a token is what an operator
# has to type into ALLOW_DEGRADED_FIXTURES, and it must be obvious from the
# token which build step to go and fix.
DEGRADED_FIXTURES=""

# ------------------------------------------------------- fixture: the agent jar

AGENT_ARG=""
if [ -f "$OUT/classes/JdkOnlyProbeAgent.class" ]; then
  printf 'Premain-Class: JdkOnlyProbeAgent\nAgent-Class: JdkOnlyProbeAgent\nCan-Retransform-Classes: true\n' \
      > "$OUT/agent-manifest.txt"
  if "$JAR" cfm "$OUT/jdkonly-probe-agent.jar" "$OUT/agent-manifest.txt" \
        -C "$OUT/classes" JdkOnlyProbeAgent.class >/dev/null 2>&1; then
    AGENT_ARG="-javaagent:$OUT/jdkonly-probe-agent.jar"
    echo "agent jar: $OUT/jdkonly-probe-agent.jar"
  else
    echo "WARNING: the agent jar did not build; the agent section will report absent"
    echo "         in EVERY arm. The arms then agree because NEITHER measured"
    echo "         anything, which is not honesty -- see the DEGRADED_FIXTURES note."
    DEGRADED_FIXTURES="$DEGRADED_FIXTURES agent-jar"
  fi
else
  # Reached when apps/probes/JdkOnlyProbeAgent.java is absent or did not produce a
  # class. This path used to print NOTHING at all -- quieter even than the
  # WARNING above, and with the identical effect on coverage.
  echo "WARNING: no JdkOnlyProbeAgent.class under $OUT/classes; no agent jar was"
  echo "         built and the agent section reports absent in EVERY arm."
  DEGRADED_FIXTURES="$DEGRADED_FIXTURES agent-class"
fi

# The MSVC fallback for the JNI fixture, and why it is not optional.
#
# `cc -shared -fPIC` is a GCC/Clang spelling. A Windows host (and a
# `windows-latest` runner with no MinGW on PATH) has no `cc` at all, so the
# branch below took the `jni-lib-no-cc` degradation and the whole `jni` section
# reported `lib=absent` in ALL THREE arms -- the self-disabling shape this
# script's own header rejects in so many words. It is worse here than that
# header says, because this script is the ONLY scheduled consumer of the probe
# corpus: with no compiler the JNI boundary on Windows was covered by nothing
# anywhere, in either mode, and the run still said PASS.
#
# MSVC is present on every Windows host that can build this repo at all --
# rustc's `x86_64-pc-windows-msvc` target links with it -- so this adds no
# prerequisite, it spells one that was already required. `cl` is deliberately
# not on PATH; `vcvars64.bat` is what puts it there, and `vswhere.exe` (a fixed,
# versionless path shipped by every VS installer since 2017) is what finds
# `vcvars64.bat` without hard-coding an edition or a version number.
#
# Writes $JNI_LIB and returns 0 on success. On any failure it explains itself
# into the same jni-build.log and returns 1, so the caller falls through to the
# SAME declared-degradation path as before -- this can turn a refusal into a
# pass, never a pass into a refusal.
build_jni_with_msvc() {
  bjm_vswhere="/c/Program Files (x86)/Microsoft Visual Studio/Installer/vswhere.exe"
  if [ ! -x "$bjm_vswhere" ]; then
    echo "no MSVC: $bjm_vswhere is not present, so cl cannot be located" \
        > "$OUT/logs/jni-build.log" 2>&1
    return 1
  fi
  bjm_root="$("$bjm_vswhere" -latest -products '*' \
      -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 \
      -property installationPath 2>/dev/null | tr -d '\r' | head -n 1)"
  if [ -z "$bjm_root" ]; then
    echo "no MSVC: vswhere found no installation carrying the x64 C++ tools" \
        > "$OUT/logs/jni-build.log" 2>&1
    return 1
  fi
  bjm_vcvars="$bjm_root/VC/Auxiliary/Build/vcvars64.bat"
  if [ ! -f "$bjm_vcvars" ]; then
    echo "no MSVC: $bjm_vcvars is missing from the installation vswhere named" \
        > "$OUT/logs/jni-build.log" 2>&1
    return 1
  fi
  bjm_bat="$OUT/build-jni-msvc.bat"
  # `cl` needs INCLUDE / LIB / PATH from vcvars64, and those survive only within
  # one cmd.exe invocation -- hence a generated batch file rather than a direct
  # call.
  #
  # The batch `cd`s into $OUT and names the DLL relatively instead of passing
  # `/Fo:"<dir>\"`. That spelling looks right and is not: a backslash
  # immediately before a closing quote escapes the quote, so cl received one
  # merged argument and answered `D8003: missing source filename` -- a compiler
  # error that reads like a broken source file rather than a quoting bug.
  # Compiling in the output directory also puts the .obj there, which is what
  # the explicit /Fo was for.
  {
    printf '@echo off\r\n'
    printf 'call "%s" >nul\r\n' "$(cygpath -w "$bjm_vcvars")"
    printf 'if errorlevel 1 exit /b 1\r\n'
    printf 'cd /d "%s"\r\n' "$(cygpath -w "$OUT")"
    printf 'if errorlevel 1 exit /b 1\r\n'
    printf 'cl /nologo /LD /O1 /I "%s\\include" /I "%s\\include\\win32" /Fe:"%s" "%s"\r\n' \
        "$(cygpath -w "$JAVA_HOME")" "$(cygpath -w "$JAVA_HOME")" \
        "$(basename "$JNI_LIB")" "$(cygpath -w "$JNI_SRC")"
  } > "$bjm_bat"
  rm -f "$JNI_LIB"
  if cmd //c "$(cygpath -w "$bjm_bat")" > "$OUT/logs/jni-build.log" 2>&1 \
      && [ -f "$JNI_LIB" ]; then
    return 0
  fi
  echo "WARNING: the MSVC JNI fixture build failed (see $OUT/logs/jni-build.log)."
  return 1
}

# --------------------------------------------------------- fixture: the JNI lib

# Built here rather than committed, because a shared object is a per-platform,
# per-toolchain artefact and a stale committed one would be loaded by all three
# arms without anyone noticing it no longer matched the C.
JNI_ARG=""
JNI_SRC="$ROOT/apps/probes/jdkonly_jni_probe.c"
if [ -f "$JNI_SRC" ]; then
  case "$(uname -s 2>/dev/null || echo unknown)" in
    Darwin) JNI_LIB="$OUT/libcratonjniprobe.dylib"; JNI_OS=darwin ;;
    MINGW*|MSYS*|CYGWIN*) JNI_LIB="$OUT/cratonjniprobe.dll"; JNI_OS=win32 ;;
    *) JNI_LIB="$OUT/libcratonjniprobe.so"; JNI_OS=linux ;;
  esac
  CC="${CC:-cc}"
  if command -v "$CC" >/dev/null 2>&1; then
    if "$CC" -shared -fPIC -O1 \
          -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/$JNI_OS" \
          -o "$JNI_LIB" "$JNI_SRC" 2>"$OUT/logs/jni-build.log"; then
      JNI_ARG="-Dcraton.probe.jnilib=$JNI_LIB"
      echo "jni lib: $JNI_LIB"
    else
      echo "WARNING: the JNI fixture did not build (see $OUT/logs/jni-build.log)."
      echo "         The jni section will report lib=absent in EVERY arm."
      DEGRADED_FIXTURES="$DEGRADED_FIXTURES jni-lib"
    fi
  elif [ "$JNI_OS" = win32 ] && build_jni_with_msvc; then
    JNI_ARG="-Dcraton.probe.jnilib=$JNI_LIB"
    echo "jni lib: $JNI_LIB (MSVC)"
  else
    echo "WARNING: no C compiler ($CC); the jni section reports lib=absent in every arm."
    DEGRADED_FIXTURES="$DEGRADED_FIXTURES jni-lib-no-cc"
  fi
else
  # Same silent path as the agent's: no C source, no warning, no JNI section,
  # and three arms agreeing about a boundary none of them crossed.
  echo "WARNING: $JNI_SRC does not exist; the jni section reports lib=absent in"
  echo "         EVERY arm."
  DEGRADED_FIXTURES="$DEGRADED_FIXTURES jni-source"
fi

# ------------------------------------------------------------------ the run

# One arm = one (label, launcher, extra-args) triple. HotSpot is first so a
# missing control is noticed before anything is compared to it.
run_arm() {
  arm_label="$1"; shift
  probe="$1"; shift
  log="$OUT/logs/$probe.$arm_label.txt"
  # 2>&1 into the transcript: a strict-mode UnsatisfiedLinkError arrives on
  # stderr, and a gate that only reads stdout scores it as a clean run.
  timeout "$TIMEOUT" "$@" > "$log" 2>&1
  rc=$?
  echo "$rc" > "$OUT/logs/$probe.$arm_label.rc"
  return $rc
}

BAD_EXIT=""
DIVERGED=""
BOTH_MODES=""
OBSERVED="$OUT/observed-keys.txt"
: > "$OBSERVED"

for probe in $PROBE_LIST; do
  echo ""
  echo "== $probe =="

  run_arm hotspot "$probe" \
      "$JAVA" $AGENT_ARG $JNI_ARG -cp "$OUT/classes" "$probe"
  hs_rc=$?

  run_arm real "$probe" \
      "$CV" --real-jdk --java-home "$JAVA_HOME" $AGENT_ARG $JNI_ARG -cp "$OUT/classes" "$probe"
  real_rc=$?

  run_arm strict "$probe" \
      "$CV" --jdk-only --java-home "$JAVA_HOME" $AGENT_ARG $JNI_ARG -cp "$OUT/classes" "$probe"
  strict_rc=$?

  echo "exit: hotspot=$hs_rc real-jdk=$real_rc jdk-only=$strict_rc"

  if [ "$hs_rc" -ne 0 ]; then
    echo "ERROR: the HotSpot CONTROL exited $hs_rc. Nothing below is a measurement"
    echo "       of CratonVM -- fix the probe or the host first."
    BAD_EXIT="$BAD_EXIT $probe/hotspot"
  fi
  [ "$real_rc" -ne 0 ] && BAD_EXIT="$BAD_EXIT $probe/real-jdk"
  [ "$strict_rc" -ne 0 ] && BAD_EXIT="$BAD_EXIT $probe/jdk-only"

  # Normalization, and its limits.
  #
  # A gate that rewrites the transcript until it matches is a gate that cannot
  # fail, and this repo has produced three of those. So exactly two things are
  # dropped, both identified by their EMITTER rather than by their content:
  #
  #   * `[cratonvm] ...`         the VM's own banner and shutdown lines
  #   * a `tracing` record        recognised by the `cratonvm_<crate>::<module>`
  #                               target it carries, after ANSI stripping
  #   * `WARNING: ...`            the JDK launcher's own warnings (the
  #                               restricted-method notice that System.load
  #                               triggers on HotSpot and not on CratonVM)
  #   * an empty line             HotSpot's restricted-method notice ends with
  #                               one, so dropping the WARNINGs and keeping the
  #                               blank would leave a diff that is purely an
  #                               artefact of the filter above
  #
  # No probe prints a line in any of those shapes — every probe line begins
  # with its section name — so nothing a probe says can be filtered by
  # accident. The count of dropped lines is REPORTED per arm: a normalizer
  # that suddenly starts eating output shows up as a number, not as a silent
  # pass.
  for a in hotspot real strict; do
    raw="$OUT/logs/$probe.$a.txt"
    sed -e 's/\x1b\[[0-9;]*[A-Za-z]//g' "$raw" | tr -d '\r' > "$OUT/logs/$probe.$a.plain"
    grep -avE '^\[cratonvm\]|cratonvm_[a-z_]+::|^WARNING: |^[[:space:]]*$' \
        "$OUT/logs/$probe.$a.plain" > "$OUT/logs/$probe.$a.norm"
    kept=$(wc -l < "$OUT/logs/$probe.$a.norm")
    total=$(wc -l < "$OUT/logs/$probe.$a.plain")
    echo "  $a: $kept probe lines, $((total - kept)) VM/launcher lines filtered"
  done

  real_diff=0
  strict_diff=0
  diff -u "$OUT/logs/$probe.hotspot.norm" "$OUT/logs/$probe.real.norm" \
      > "$OUT/logs/$probe.real.diff" 2>&1 || real_diff=1
  diff -u "$OUT/logs/$probe.hotspot.norm" "$OUT/logs/$probe.strict.norm" \
      > "$OUT/logs/$probe.strict.diff" 2>&1 || strict_diff=1

  # Record what diverged, per arm, as `probe/arm/section` — the ratchet's unit.
  for a in real strict; do
    divergent_keys < "$OUT/logs/$probe.$a.diff" \
      | sed "s|^|$probe/$a/|" >> "$OBSERVED"
  done

  if [ "$real_diff" -eq 0 ] && [ "$strict_diff" -eq 0 ]; then
    echo "transcript: byte-identical to HotSpot in both modes"
    continue
  fi

  if [ "$real_diff" -eq 1 ] && [ "$strict_diff" -eq 1 ]; then
    echo "DIVERGED in BOTH modes -- this is a compatibility defect, not a"
    echo "  strict-mode one. --jdk-only did not introduce it."
    BOTH_MODES="$BOTH_MODES $probe"
  elif [ "$strict_diff" -eq 1 ]; then
    echo "DIVERGED under --jdk-only ONLY -- a strict-mode defect."
  else
    echo "DIVERGED under --real-jdk ONLY -- strict mode is right and permissive"
    echo "  mode is wrong, which is worth a second look before filing."
  fi
  DIVERGED="$DIVERGED $probe"
  sed -n '1,60p' "$OUT/logs/$probe.strict.diff"
  [ "$strict_diff" -eq 0 ] && sed -n '1,60p' "$OUT/logs/$probe.real.diff"
done

# ------------------------------------------------------------------- verdict

echo ""
echo "================ strict corpus verdict ================"
echo "probes:       $PROBE_LIST"
echo "logs:         $OUT/logs"
if [ -n "$BAD_EXIT" ]; then
  echo "INCOMPLETE:  $BAD_EXIT"
fi
if [ -n "$DIVERGED" ]; then
  echo "DIVERGED:    $DIVERGED"
fi
if [ -n "$BOTH_MODES" ]; then
  echo "  (both modes, so pre-existing:$BOTH_MODES)"
fi
if [ -n "$DEGRADED_FIXTURES" ]; then
  echo "DEGRADED:   $DEGRADED_FIXTURES"
fi

# An incomplete arm is never baselined and always fails, before the ratchet
# gets a say: a truncated transcript's divergent set is meaningless.
if [ -n "$BAD_EXIT" ]; then
  echo "RESULT: FAIL -- an arm did not complete. Never baselined: a hang or a"
  echo "        crash is not a known-acceptable transcript."
  exit 4
fi

# ------------------------------------- undeclared degradation refuses to score
#
# BEFORE the baseline is read or written, because both readings are wrong under
# a degraded run: gating against a baseline frozen with the fixtures present
# would compare a full baseline to a hollowed-out run, and --update-baseline
# would freeze the hollowed-out set as the new normal, quietly deleting the
# agent and JNI rows from the ratchet.
#
# ALLOW_DEGRADED_FIXTURES is `1`/`all` for a blanket allowance, or a
# space/comma-separated list of the tokens printed above. The list spelling is
# the one to use: a blanket `1` also swallows a fixture that used to build.
if [ -n "$DEGRADED_FIXTURES" ]; then
  UNDECLARED="$(undeclared_degradations "$DEGRADED_FIXTURES" "${ALLOW_DEGRADED_FIXTURES:-0}")"

  if [ -n "$UNDECLARED" ]; then
    echo ""
    echo "UNDECLARED DEGRADED FIXTURES:$UNDECLARED"
    echo "  Each of these switches a whole SECTION off in EVERY arm at once:"
    echo "    agent-jar / agent-class   -> JdkOnlyPlatformProbe's 'agent' section"
    echo "    jni-lib / jni-lib-no-cc / jni-source"
    echo "                              -> JdkOnlyPlatformProbe's 'jni' section"
    echo "  The arms then agree, no section diverges, and the ratchet cannot"
    echo "  fire -- so a PASS here would mean 'nothing was measured', not"
    echo "  'nothing broke'. Three arms that all failed to build agree with each"
    echo "  other; that is the absence of evidence, not evidence."
    echo "  Nothing else in the tree covers those sections: no SUITE= value of"
    echo "  regression-suite/run.sh schedules a single probe."
    echo "  Fix the fixture, or DECLARE the gap so the run says out loud what it"
    echo "  did not cover:"
    echo "    ALLOW_DEGRADED_FIXTURES=\"$(echo $UNDECLARED)\" bash $0"
    echo "RESULT: REFUSED -- a fixture did not build, so its section is absent in"
    echo "        every arm and the agreement between them is vacuous."
    exit 2
  fi

  echo ""
  echo "DECLARED FIXTURE GAP:$DEGRADED_FIXTURES"
  echo "  ALLOW_DEGRADED_FIXTURES=${ALLOW_DEGRADED_FIXTURES:-0} was set, so this run"
  echo "  continues. It did NOT cover the sections those fixtures feed; whatever"
  echo "  it reports below is a verdict on the REST of the corpus only."
fi

sort -u "$OBSERVED" -o "$OBSERVED"
n_obs=$(grep -c . "$OBSERVED" || true)

if [ "$UPDATE" -eq 1 ]; then
  mkdir -p "$(dirname "$BASELINE")"
  {
    echo "# Strict-corpus divergence baseline — jdk $FEATURE, $OSKEY"
    echo "#"
    echo "# GENERATED by scripts/jdk-only-strict-probes.sh --update-baseline."
    echo "# Never hand-edited: every line is a (probe, arm, section) that a real"
    echo "# three-arm run measured as diverging from the HotSpot control."
    echo "#"
    echo "# note: $NOTE"
    if [ -n "$DEGRADED_FIXTURES" ]; then
      echo "#"
      echo "# WARNING: frozen from a DEGRADED run. These fixtures did not build, so"
      echo "# the sections they feed were absent in every arm and contributed no"
      echo "# rows to the set below:$DEGRADED_FIXTURES"
      echo "# Re-freeze on a host where they build before trusting this key."
    fi
    echo "#"
    echo "# The gate fails when this set GROWS. A line that stops diverging is"
    echo "# reported and passes, so a fix is never blocked by the gate that"
    echo "# measured it — re-freeze afterwards to keep the ratchet tight."
    cat "$OBSERVED"
  } > "$BASELINE"
  echo "baseline updated: $BASELINE ($n_obs entries)"
  echo "RESULT: BASELINE WRITTEN -- re-run without --update-baseline to gate."
  exit 0
fi

if [ ! -f "$BASELINE" ]; then
  echo "RESULT: REFUSED -- no baseline at $BASELINE."
  echo "  The divergent set is a property of the image and the platform, so a"
  echo "  baseline from another key cannot adjudicate this one. Produce it with"
  echo "  a real run:"
  echo "    JAVA_HOME=$JAVA_HOME CV=$CV bash scripts/jdk-only-strict-probes.sh \\"
  echo "        --update-baseline --note \"first baseline for $FEATURE-$OSKEY\""
  echo "  Refusing is not a pass: $n_obs divergent section(s) were measured and"
  echo "  nothing has adjudicated them."
  exit 2
fi

grep -vE '^\s*(#|$)' "$BASELINE" | sort -u > "$OUT/baseline-keys.txt"
NEW="$(comm -23 "$OBSERVED" "$OUT/baseline-keys.txt")"
GONE="$(comm -13 "$OBSERVED" "$OUT/baseline-keys.txt")"
n_base=$(grep -c . "$OUT/baseline-keys.txt" || true)

echo "divergent sections: $n_obs observed, $n_base baselined"
if [ -n "$GONE" ]; then
  echo ""
  echo "NO LONGER DIVERGING (not a failure — re-freeze to keep the ratchet tight):"
  printf '%s\n' "$GONE" | sed 's/^/  - /'
fi
if [ -n "$NEW" ]; then
  echo ""
  echo "NEW DIVERGENCES — the ratchet fired:"
  printf '%s\n' "$NEW" | sed 's/^/  + /'
  echo ""
  echo "Each is a section that matched the HotSpot control when the baseline was"
  echo "frozen and does not now. The per-arm diffs are in $OUT/logs/*.diff."
  echo "If the change is intended, re-freeze:"
  echo "    bash scripts/jdk-only-strict-probes.sh --update-baseline --note \"...\""
  echo "RESULT: FAIL -- the strict corpus regressed against its baseline."
  exit 5
fi

if [ -n "$DEGRADED_FIXTURES" ]; then
  echo "RESULT: PASS (DEGRADED:$DEGRADED_FIXTURES) -- every arm completed and no"
  echo "        section diverged that the baseline does not already carry, but the"
  echo "        sections fed by those fixtures were not measured at all."
  exit 0
fi

echo "RESULT: PASS -- every arm completed and no section diverged that the"
echo "        baseline does not already carry."
exit 0
