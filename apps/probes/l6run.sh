#!/usr/bin/env bash
# l6run.sh <Probe> [more...] — the differential runner for lane L6
# (`java.net`, `javax.net.ssl`, `javax.crypto`, `sun.security.*`).
#
#   oracle    HotSpot   ($JDK, default /data/jdkimages/jdk25-linux/jdk-25.0.4+7)
#   strict    cratonvm --jdk-only   ($CV)
#   trial     cratonvm --jdk-only   ($CV2, optional — the A/B's other arm)
#
# Modelled on `l5run.sh`. Two differences this lane needs.
#
# **No `--add-exports`.** Nothing in L6 reaches `jdk.internal.*`; every probe
# here compiles against the exported `java.net`/`javax.net.ssl` surface, and
# adding the flag would only hide a probe that had started to need it.
#
# **A second VM arm rather than a dial arm.** L6's fixes live in the natives,
# not behind `CRATONVM_ENFORCE_NATIVE_SHADOW`, so the honest comparison is two
# BINARIES: `d(hs,strict)` against `d(hs,trial)`. A leaked dial row is not
# evidence here, and there is no dial to leak.
#
# Check the `rows` line, printed before the diff, for the reason `l4run.sh`
# and `l5run.sh` do: a run that died partway produces a short file whose
# missing tail `diff` reports as ordinary `<` lines, which reads as a wall of
# differences rather than as the crash it is.
set +e
export MSYS2_ARG_CONV_EXCL='*'
export MSYS_NO_PATHCONV=1

W="$(git -C "$(dirname "${BASH_SOURCE[0]}")" rev-parse --show-toplevel 2>/dev/null)"
if [ -z "$W" ] || [ ! -d "$W/apps/probes" ]; then
  echo "l6run.sh: cannot resolve the repository root — run it from inside its worktree." >&2
  exit 3
fi
CV="${CV:-$W/target/release/cratonvm}"
[ -x "$CV" ] || CV="$W/target/release/cratonvm.exe"
CV2="${CV2:-}"
JDK="${JDK:-${JAVA_HOME:-/data/jdkimages/jdk25-linux/jdk-25.0.4+7}}"
OUT="${OUT:-${TMPDIR:-/tmp}/l6out}"
mkdir -p "$OUT" "$W/apps/probes/out"
cd "$W" || exit 1
export CRATONVM_DISABLE_DEFAULT_WATCHDOG=1

for C in "$@"; do
  if ! "$JDK/bin/javac" -d "$W/apps/probes/out" "$W/apps/probes/$C.java" 2>"$OUT/$C.javac"; then
    echo "=== $C   JAVAC-FAILED"
    head -5 "$OUT/$C.javac"
    continue
  fi
  timeout 900 "$JDK/bin/java" -cp "$W/apps/probes/out" "$C" > "$OUT/$C.oracle" 2>/dev/null
  o=$?
  timeout 900 "$CV" --java-home "$JDK" --jdk-only -cp "$W/apps/probes/out" "$C" > "$OUT/$C.strict" 2>/dev/null
  s=$?
  t=-1
  if [ -n "$CV2" ]; then
    timeout 900 "$CV2" --java-home "$JDK" --jdk-only -cp "$W/apps/probes/out" "$C" > "$OUT/$C.trial" 2>/dev/null
    t=$?
  fi
  echo "=== $C   rc oracle=$o strict=$s trial=$t"
  echo "    lines  oracle=$(wc -l < "$OUT/$C.oracle")  strict=$(wc -l < "$OUT/$C.strict")$([ -n "$CV2" ] && echo "  trial=$(wc -l < "$OUT/$C.trial")")"
  echo "    rows   oracle $(grep -a '^rows ' "$OUT/$C.oracle") | strict $(grep -a '^rows ' "$OUT/$C.strict")$([ -n "$CV2" ] && echo " | trial $(grep -a '^rows ' "$OUT/$C.trial")")"
  ds=$(diff "$OUT/$C.oracle" "$OUT/$C.strict" | grep -ac '^[<>]')
  diff "$OUT/$C.oracle" "$OUT/$C.strict" > "$OUT/$C.strict.diff"
  if [ -n "$CV2" ]; then
    dt=$(diff "$OUT/$C.oracle" "$OUT/$C.trial" | grep -ac '^[<>]')
    diff "$OUT/$C.oracle" "$OUT/$C.trial" > "$OUT/$C.trial.diff"
    echo "    DIFF   d(hs,strict)=$ds  d(hs,trial)=$dt  delta=$((dt - ds))"
  else
    echo "    DIFF   d(hs,strict)=$ds"
  fi
done
echo "L6RUN-DONE"
