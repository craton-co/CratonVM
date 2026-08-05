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
#   2  a prerequisite is missing, or there is no baseline for this
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
  return 0
}
if ! selftest; then
  echo "RESULT: REFUSED -- the gate cannot adjudicate because its own logic is broken."
  exit 2
fi
echo "self-test: the ratchet fires on a new section and not on a baselined one"

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

# An incomplete arm is never baselined and always fails, before the ratchet
# gets a say: a truncated transcript's divergent set is meaningless.
if [ -n "$BAD_EXIT" ]; then
  echo "RESULT: FAIL -- an arm did not complete. Never baselined: a hang or a"
  echo "        crash is not a known-acceptable transcript."
  exit 4
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

echo "RESULT: PASS -- every arm completed and no section diverged that the"
echo "        baseline does not already carry."
exit 0
