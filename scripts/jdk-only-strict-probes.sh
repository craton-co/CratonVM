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
# Usage:
#   JAVA_HOME=/path/to/real/jdk25 \
#   CV=target/release/cratonvm \
#   [OUT=target/jdk-only-strict-probes] \
#   [TIMEOUT=300] \
#   [PROBE_LIST="JdkOnlyCensusLoadProbe ..."] \
#   scripts/jdk-only-strict-probes.sh
#
# Exit codes:
#   0  every arm completed and every CratonVM transcript matched HotSpot
#   2  a prerequisite is missing (no JAVA_HOME, no javac, no cratonvm) -- nothing ran
#   3  a probe failed to compile
#   4  an arm did not complete (non-zero exit, timeout, or crash)
#   5  a transcript diverged from the HotSpot control
#
# 4 and 5 are separate on purpose: a truncated run and a wrong answer need
# different triage, and collapsing them into "failed" is how a hang gets filed
# as a diff.
set -u

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${OUT:-$ROOT/target/jdk-only-strict-probes}"
TIMEOUT="${TIMEOUT:-300}"

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

# ------------------------------------------------------------------ the corpus

# JdkOnlyIcHotProbe is deliberately absent: it exists to drive the refusal
# counters under a JIT-hot workload, and its transcript is a throughput
# artefact, not a value to diff. scripts/jdk-only-measure-refusals-and-overlays.sh
# is where it belongs.
PROBE_LIST="${PROBE_LIST:-JdkOnlyCensusLoadProbe JdkOnlyBreadthProbe JdkOnlyPlatformProbe}"

SRCS=""
for p in $PROBE_LIST; do
  if [ ! -f "$ROOT/probes/$p.java" ]; then
    echo "ERROR: probes/$p.java does not exist."
    exit 3
  fi
  SRCS="$SRCS $ROOT/probes/$p.java"
done
# The agent is compiled with the probes so JdkOnlyPlatformProbe's reflective
# lookup has something to find; it is never listed as a probe itself.
if [ -f "$ROOT/probes/JdkOnlyProbeAgent.java" ]; then
  SRCS="$SRCS $ROOT/probes/JdkOnlyProbeAgent.java"
fi

echo "== compiling the strict corpus =="
if ! "$JAVAC" -d "$OUT/classes" $SRCS 2>&1; then
  echo "ERROR: the probe corpus did not compile."
  exit 3
fi

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
    echo "         in EVERY arm, so the arms still agree and the gate stays honest."
  fi
fi

# --------------------------------------------------------- fixture: the JNI lib

# Built here rather than committed, because a shared object is a per-platform,
# per-toolchain artefact and a stale committed one would be loaded by all three
# arms without anyone noticing it no longer matched the C.
JNI_ARG=""
JNI_SRC="$ROOT/probes/jdkonly_jni_probe.c"
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
    fi
  else
    echo "WARNING: no C compiler ($CC); the jni section reports lib=absent in every arm."
  fi
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

if [ -n "$BAD_EXIT" ]; then
  echo "RESULT: NOT GREEN -- an arm did not complete."
  exit 4
fi
if [ -n "$DIVERGED" ]; then
  echo "RESULT: NOT GREEN -- a transcript diverged from the HotSpot control."
  exit 5
fi
echo "RESULT: GREEN -- every arm completed and matched HotSpot."
exit 0
