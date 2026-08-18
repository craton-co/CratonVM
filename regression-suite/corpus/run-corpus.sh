#!/usr/bin/env bash
###############################################################################
# run-corpus.sh - real-application corpus driver for CratonVM.
#
# The regression suite (regression-suite/run.sh) is 72 small deterministic
# vectors. It is a good gate and it licenses almost nothing about real
# applications, which is the evidence Phase 2 of the JDK-only roadmap needs:
# native bridges are retired by FAMILY, and only a real corpus can adjudicate
# whether a family's retirement broke anything.
#
# This driver runs a named real-application workload under a chosen CratonVM
# binary and mode, runs the SAME workload on HotSpot 25 as the oracle, and
# reports an honest verdict. See regression-suite/corpus/README.md for the
# semantics and docs/feature-designs/jdk-only-corpus-runner.md for the design
# and the on-disk corpus inventory.
#
#   IT IS NOT A BENCHMARK. Wall-clock throughput on this shared, loaded host
#   is worthless -- three clean interleaved A/B pairs have previously turned
#   out to be pure load drift. This driver adjudicates correctness only:
#   pass / fail / diverge. It records elapsed milliseconds in the TSV solely
#   so a 4-minute workload can be told from a 4-second one when choosing a
#   timeout, and every consumer must treat that column as non-metric.
#
#   THE INSTRUMENT IS PART OF THE SUBJECT. Every measured defect this driver
#   has produced so far was a harness defect reported as a VM finding (both
#   arms running in the invoking cwd; a shutdown-hook-sourced marker inside
#   the comparison key; an @argfile path that a native java.exe cannot open).
#   `selfcheck` below is the standing positive control that this machinery can
#   still go red; run it before believing a red run, and after editing.
#
# Usage:
#   bash run-corpus.sh list
#   bash run-corpus.sh info      <corpus>
#   bash run-corpus.sh discover  <corpus> [--limit N]
#   bash run-corpus.sh run       <corpus> [options]
#   bash run-corpus.sh selfcheck [--unit-only]
#
# run options:
#   --class  <FQCN>      workload class (default: the corpus's default)
#   --classes-from <F>   file of workload classes, one FQCN per line
#   --mode   <MODE>      real-jdk (default) | jdk-only | synthetic-jdk | default
#   --cv     <PATH>      CratonVM binary (default: $ROOT/target/release/cratonvm[.exe])
#   --jdk    <PATH>      JDK home for --java-home and for the oracle (default: $JAVA_HOME)
#   --timeout <SECONDS>  per-arm timeout (default: 600)
#   --out    <DIR>       results directory (default: regression-suite/corpus/out)
#   --no-oracle          skip the HotSpot arm; every verdict becomes UNADJUDICATED
#   --cv-args "<flags>"  extra flags for the CratonVM arm ONLY
#
# Exit codes:
#   0  every workload adjudicated AGREE (and at least one workload ran)
#   1  at least one workload DIVERGE, or the CratonVM arm failed
#   2  precondition failure (corpus missing/not built, no VM, no JDK, empty
#      workload list, oracle unusable or vacuous, HARNESS-ERROR) -- nothing
#      was adjudicated
#   3  usage error
###############################################################################
set -u
set -o pipefail
# `set -e` is deliberately NOT used. Half this file's control flow is `grep`
# whose non-match (exit 1) is the ordinary case, and an -e that aborts on the
# first "no match" would turn every clean log into a silent early exit. Failure
# is handled explicitly, by `die`.

# Git Bash on Windows rewrites anything that looks like a POSIX path inside an
# argument. Classpaths, `@argfile` arguments and Java class names all look like
# paths to it. regression-suite/run.sh:41 disables the same two conversions for
# the same reason; without them a `-cp` value silently becomes a mangled
# Windows path list and every workload fails identically.
export MSYS2_ARG_CONV_EXCL='*'
export MSYS_NO_PATHCONV=1

# `pwd -W` (MSYS/Git Bash) yields the NATIVE path, C:/... rather than /c/....
# Both forms are valid to bash, but MSYS_NO_PATHCONV=1 above stops Git Bash
# rewriting arguments on the way out to a native process -- so a /c/... path
# handed to javac.exe or java.exe arrives verbatim and is not a path at all.
# That is not a hypothetical: the first run of this script died with
# `error: file not found: \c\craton\...\CorpusMain.java`. Everything this
# script passes to a native tool must therefore start life native.
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && { pwd -W 2>/dev/null || pwd; })"
ROOT="$(git -C "$HERE" rev-parse --show-toplevel 2>/dev/null || (cd "$HERE/../.." && { pwd -W 2>/dev/null || pwd; }))"
# CORPORA_D and BUILD are overridable so `selfcheck` can point the REAL code
# path at a throwaway corpus without touching the tracked one.
CORPORA_D="${CORPORA_D:-$HERE/corpora.d}"
BUILD="${BUILD:-$HERE/build}"
OUT="${OUT:-$HERE/out}"

TIMEOUT="${TIMEOUT:-600}"
MODE="${MODE:-real-jdk}"
CV="${CV:-}"
JDK="${JDK:-${JAVA_HOME:-}}"
NO_ORACLE=0
CV_ARGS="${CV_ARGS:-}"
LIMIT=0
WORKCLASS=""
CLASSES_FROM=""

die()  { echo "ERROR: $*" >&2; exit "${2:-2}"; }
usage_die() { echo "ERROR: $*" >&2; echo "Run 'bash run-corpus.sh' with no arguments for usage." >&2; exit 3; }
log()  { echo "[$(date +%H:%M:%S)] $*" >&2; }

# Classpath separator. Everything else in this file uses forward slashes even
# on Windows: both java.exe and cratonvm.exe accept them, and a forward-slash
# path survives `@argfile` tokenization unescaped, which a backslash path does
# not (java's argfile grammar treats `\` as an escape character, and
# vm-cli/src/main.rs:1173 `tokenize_argfile` does the same inside quotes).
case "$(uname -s 2>/dev/null || echo unknown)" in
  MINGW*|MSYS*|CYGWIN*) SEP=';' ;;
  *)                    SEP=':' ;;
esac

# Print an EXISTING directory in the form a native tool can open. Measured
# 2026-08-12: passing `--out /c/Users/.../scratch` put the run's `cp.args` at a
# POSIX path, java.exe answered `Error: could not open '/c/Users/...'`, both
# arms scored NOSTART and the row was reported as ORACLE-UNUSABLE -- a harness
# path bug wearing a corpus verdict. Every caller-supplied directory that ends
# up inside an argument to java.exe/cratonvm.exe goes through here.
native_dir() {
  local d="$1"
  [ -d "$d" ] || { printf '%s' "$d"; return 0; }
  ( cd "$d" && { pwd -W 2>/dev/null || pwd; } )
}

# --- classpath helpers, available to every corpora.d/*.sh -------------------

CP=""
cp_reset() { CP=""; }
cp_add() {
  # Append one classpath entry, but only if it actually exists. A composed
  # classpath that silently contains three non-existent directories produces
  # a NoClassDefFoundError that reads exactly like a VM defect.
  local p="$1"
  [ -e "$p" ] || return 0
  if [ -z "$CP" ]; then CP="$p"; else CP="$CP$SEP$p"; fi
}
cp_add_jars() {
  # Non-recursive: every *.jar directly inside $1.
  local d="$1" j
  [ -d "$d" ] || return 0
  for j in "$d"/*.jar; do [ -e "$j" ] && cp_add "$j"; done
}
cp_add_jars_r() {
  # Recursive. Used by the multi-module gradle/maven corpora whose dependency
  # jars are scattered across per-module build directories.
  local d="$1" j
  [ -d "$d" ] || return 0
  while IFS= read -r j; do [ -n "$j" ] && cp_add "$j"; done < <(find "$d" -name '*.jar' 2>/dev/null | sort)
}
cp_count() { [ -z "$CP" ] && { echo 0; return; }; printf '%s' "$CP" | tr "$SEP" '\n' | grep -c .; }

# --- corpus definition loading ---------------------------------------------

CORPUS=""
load_corpus() {
  local name="$1" def="$CORPORA_D/$1.sh"
  [ -f "$def" ] || die "unknown corpus '$name' -- no $def. Run 'bash run-corpus.sh list'."
  # Defaults, so a definition may omit anything it does not need.
  CORPUS_DESC=""; CORPUS_KIND=main; CORPUS_DEFAULT_CLASS=""
  CORPUS_CONFIDENCE=unverified; CORPUS_ROOT_CANDIDATES=""; CORPUS_NOTE=""
  CORPUS_RUNNER_CLASS=""; CORPUS_RUNNER_SRC=""
  # Workspace state the workload WRITES into its working directory and which
  # must not survive into the next arm. Space-separated, relative to the
  # corpus workdir. Empty by default; see `clean_arm_state`.
  CORPUS_CLEAN_PATHS=""
  corpus_is_built() { return 1; }
  corpus_classpath() { return 1; }
  corpus_discover() { return 1; }
  corpus_workdir() { echo "$1"; }
  # shellcheck disable=SC1090
  . "$def" || die "failed to load corpus definition $def"
  CORPUS="$name"
}

# Resolve which of the candidate roots is actually BUILT. This is the whole
# point of the candidate list. `apps/` is gitignored, the corpus trees exist in
# two places on this host, and the two are NOT interchangeable: for h2 the
# built classes live under one root while the dependency jars live under the
# other, and the unbuilt root contains a target/classes holding exactly one
# .class file. A runner that resolved a corpus by directory EXISTENCE would
# pick the decoy and report 200 identical NoClassDefFoundErrors.
CORPUS_ROOT=""
resolve_root() {
  local r
  for r in $CORPUS_ROOT_CANDIDATES; do
    [ -d "$r" ] || continue
    if corpus_is_built "$r"; then CORPUS_ROOT="$r"; return 0; fi
  done
  CORPUS_ROOT=""
  return 1
}

root_report() {
  local r
  echo "  candidate roots:"
  for r in $CORPUS_ROOT_CANDIDATES; do
    if [ ! -d "$r" ]; then echo "    ABSENT     $r"
    elif corpus_is_built "$r"; then echo "    BUILT      $r"
    else echo "    NOT-BUILT  $r"; fi
  done
}

# Remove the per-run state a corpus declares, BEFORE each arm. Two arms that
# share a working directory do not share a workload unless this happens: the
# CratonVM arm runs first, and whatever it leaves behind is the oracle's input.
# Measured on H2: `TestBackup` fails with `MVStoreException: Chunk 2 not found`
# intermittently in BOTH arms when a `data/` directory carries over, i.e. the
# harness manufactures a failure that is then attributed to whatever VM setting
# is under test.
clean_arm_state() {
  local wd="$1" p
  for p in ${CORPUS_CLEAN_PATHS:-}; do
    case "$p" in
      ""|"."|".."|/*|*..*|*'*'*)
        log "REFUSING unsafe CORPUS_CLEAN_PATHS entry '$p' (must be a plain relative path under the workdir)"
        continue ;;
    esac
    rm -rf -- "$wd/$p" 2>/dev/null || log "warning: could not remove $wd/$p"
  done
}

# --- toolchain resolution ---------------------------------------------------

resolve_tools() {
  [ -n "$JDK" ] || die "no JDK: set --jdk or JAVA_HOME"
  JAVAC="$JDK/bin/javac.exe"; HS="$JDK/bin/java.exe"
  [ -x "$JAVAC" ] || { JAVAC="$JDK/bin/javac"; HS="$JDK/bin/java"; }
  [ -x "$JAVAC" ] || die "no javac under $JDK/bin"
  [ -x "$HS" ]    || die "no java under $JDK/bin"

  if [ -z "$CV" ]; then
    local c
    for c in "$ROOT/target/release/cratonvm.exe" "$ROOT/target/release/cratonvm"; do
      [ -x "$c" ] && { CV="$c"; break; }
    done
  else
    [ -x "$CV" ] || { case "$CV" in *.exe) [ -x "${CV%.exe}" ] && CV="${CV%.exe}" ;; esac; }
  fi
}

mode_flags() {
  case "$MODE" in
    real-jdk)      echo "--real-jdk" ;;
    jdk-only)      echo "--jdk-only" ;;
    synthetic-jdk) echo "--synthetic-jdk" ;;
    default)       echo "" ;;
    *) usage_die "--mode '$MODE' is not one of real-jdk|jdk-only|synthetic-jdk|default" ;;
  esac
}

# Compile CorpusMain once, plus the corpus's JUnit runner if it declares one.
# javac only -- this script never builds the VM.
#
# $1 is the classpath the runner source needs (the JUnit platform launcher
# lives on the corpus classpath, not here). CorpusMain itself has no
# dependencies beyond java.base on purpose: it must be compilable even for a
# corpus whose own classpath is broken, because "the wrapper would not
# compile" and "the corpus is not built" are different failures.
ensure_wrapper() {
  local runner_cp="${1:-}"
  mkdir -p "$BUILD"
  if [ ! -f "$BUILD/CorpusMain.class" ] || [ "$HERE/CorpusMain.java" -nt "$BUILD/CorpusMain.class" ]; then
    log "compiling CorpusMain.java -> $BUILD"
    "$JAVAC" -d "$BUILD" "$HERE/CorpusMain.java" || die "javac failed on CorpusMain.java"
  fi
  if [ -n "$CORPUS_RUNNER_SRC" ]; then
    [ -f "$CORPUS_RUNNER_SRC" ] || die "corpus '$CORPUS' declares CORPUS_RUNNER_SRC='$CORPUS_RUNNER_SRC' which does not exist"
    local cls="$BUILD/$CORPUS_RUNNER_CLASS.class"
    if [ ! -f "$cls" ] || [ "$CORPUS_RUNNER_SRC" -nt "$cls" ]; then
      log "compiling $(basename "$CORPUS_RUNNER_SRC") -> $BUILD"
      "$JAVAC" -d "$BUILD" -cp "$runner_cp" "$CORPUS_RUNNER_SRC" \
        || die "javac failed on $CORPUS_RUNNER_SRC -- the JUnit platform launcher is probably not on this corpus's composed classpath"
    fi
  fi
}

# --- argfile ----------------------------------------------------------------
#
# The classpath goes in an @argfile, never on the command line, and this is a
# hard requirement rather than tidiness: Windows caps a command line at 32767
# characters and the hibernate-orm classpath alone is 40174 bytes. Passing it
# inline does not fail cleanly -- it fails as a truncated classpath, i.e. as a
# fake linkage error. Both arms support @argfile: java since 9, and CratonVM at
# vm-cli/src/main.rs:1132 `expand_argfiles`.
write_argfile() {
  local f="$1" cp="$2"
  # Quoted so a path containing a space survives both tokenizers. Forward
  # slashes only, so no character inside the quotes is an escape.
  printf -- '-cp "%s"\n' "$cp" > "$f"
}

# The argfile is the one input BOTH arms share, and a native process that
# cannot open it fails in a way that reads exactly like a workload failure.
# Prove the oracle can read it and start a JVM BEFORE any workload runs, so
# that shape exits 2 as a precondition instead of producing N unadjudicated
# rows that a reader has to reverse-engineer.
preflight_argfile() {
  local argf="$1" outf="$2" rc o
  o="$("$HS" "@$argf" -version 2>&1)"; rc=$?
  printf '%s\n' "$o" > "$outf"
  if [ "$rc" -ne 0 ]; then
    echo "$o" >&2
    die "PRECONDITION: the oracle cannot start with this run's @argfile ($argf, rc=$rc). Nothing was run. If the message names a '/c/...' path, the results directory is a POSIX path and java.exe cannot open it."
  fi
  return 0
}

# --- exit-status decoding ----------------------------------------------------
#
# Four different things produce a non-zero status here and they are NOT the
# same finding. Folding them into one bucket is how "the VM is slow" gets
# written down for what was a SIGSEGV, and how a missing binary gets written
# down as a classpath problem:
#
#   124         GNU timeout expired -- WE killed it at the wall
#   125/126/127 `timeout` itself failed: bad interval, not executable, not
#               found. A harness/configuration fault, never a VM answer.
#   128+N       died of signal N (139=SIGSEGV, 134=SIGABRT, 137=SIGKILL)
#   >=0xC0000000 Windows NTSTATUS; 0xC0000005 is the access violation that is
#               the same event as SIGSEGV.
# All four are confirmed on this host (probed 2026-08-12: /nonexistent -> 127,
# `timeout abc` -> 125, expiry -> 124, `kill -SEGV $$` -> 139).
decode_status() {
  local rc="$1"
  case "$rc" in
    -1)  echo "not-run" ; return ;;
    0)   echo "exit=0" ; return ;;
    124) echo "timeout=124(expired-at-wall)" ; return ;;
    125) echo "harness=125(timeout-itself-failed)" ; return ;;
    126) echo "harness=126(command-not-executable)" ; return ;;
    127) echo "harness=127(command-not-found)" ; return ;;
  esac
  if [ "$rc" -ge 3221225472 ] 2>/dev/null; then
    local hex; hex="$(printf '0x%X' "$rc" 2>/dev/null || echo "?")"
    case "$rc" in
      3221225477) echo "ntstatus=$hex(ACCESS_VIOLATION)" ;;
      3221225725) echo "ntstatus=$hex(STACK_OVERFLOW)" ;;
      3221226505) echo "ntstatus=$hex(STACK_BUFFER_OVERRUN/fail-fast)" ;;
      3221225786) echo "ntstatus=$hex(CONTROL_C_EXIT)" ;;
      *)          echo "ntstatus=$hex" ;;
    esac
    return
  fi
  if [ "$rc" -gt 128 ] && [ "$rc" -lt 193 ]; then
    local n=$((rc-128)) nm
    case "$n" in
      1) nm=SIGHUP ;; 2) nm=SIGINT ;; 4) nm=SIGILL ;; 6) nm=SIGABRT ;;
      7) nm=SIGBUS ;; 8) nm=SIGFPE ;; 9) nm=SIGKILL ;; 11) nm=SIGSEGV ;;
      13) nm=SIGPIPE ;; 15) nm=SIGTERM ;; *) nm="SIG$n" ;;
    esac
    echo "signal=$n($nm)"
    return
  fi
  echo "exit=$rc"
}

# Seconds between an arm's LAST byte of output and the moment it was killed.
# This is the whole difference between "hung" and "slower than the cap": a
# process still writing when the wall arrived was making progress; one that
# printed nothing for the last several minutes was not. Measured on the
# bc-java hash2curve row: killed at 1500 s having last written 552 s earlier.
# Returns -1 when it cannot be determined -- which is reported as UNKNOWN and
# never silently as either answer.
log_silence_secs() {
  local logf="$1" end_epoch="$2" m d
  [ -f "$logf" ] || { echo -1; return; }
  m="$(stat -c %Y "$logf" 2>/dev/null)" || m=""
  case "$m" in ''|*[!0-9]*) echo -1; return ;; esac
  case "$end_epoch" in ''|*[!0-9]*) echo -1; return ;; esac
  d=$((end_epoch - m))
  [ "$d" -lt 0 ] && d=0
  echo "$d"
}

# --- classification ---------------------------------------------------------
#
# States, never merged, because they have disjoint suspects:
#
#   RAN              started and reached a terminal marker
#   NOSTART          no CORPUS-START: a classpath/launcher problem, NOT a VM answer
#   CRASH            the LOG carries crash evidence (SIGSEGV text, rust panic, fatal)
#   SIGNAL           the EXIT STATUS says it died: 128+N, or a Windows NTSTATUS
#   TIMEOUT-STALLED  killed at the wall having printed nothing for a long time
#   TIMEOUT-BUSY     killed at the wall while still writing output
#   TIMEOUT-UNKNOWN  killed at the wall, silence could not be measured
#   LAUNCH-FAILED    `timeout`/the binary never ran: 125/126/127 with no markers.
#                    This is a HARNESS fault and is never scored against the VM.
#
# `classify_arm <rc> <logfile> <cap_seconds> <end_epoch_seconds>`
classify_arm() {
  local rc="$1" logf="$2" cap="${3:-0}" end_epoch="${4:-0}"

  # Harness/launcher faults first. They used to fall through to NOSTART, whose
  # note sends the reader to the corpus's classpath for what is actually a bad
  # --timeout value or a binary that is not there.
  if [ "$rc" -ge 125 ] && [ "$rc" -le 127 ] && ! grep -qa '^CORPUS-' "$logf" 2>/dev/null; then
    echo LAUNCH-FAILED; return
  fi

  # The wall. Only 124 is OUR kill: `timeout` without -k never produces 137,
  # so a 137 is somebody else's SIGKILL (the OOM killer, a stray pkill) and
  # calling it TIMEOUT would attribute an external kill to slowness.
  if [ "$rc" -eq 124 ]; then
    local silent thresh
    silent="$(log_silence_secs "$logf" "$end_epoch")"
    thresh=$(( cap / 10 )); [ "$thresh" -lt 30 ] && thresh=30
    if [ "$silent" -lt 0 ]; then echo TIMEOUT-UNKNOWN
    elif [ "$silent" -ge "$thresh" ]; then echo TIMEOUT-STALLED
    else echo TIMEOUT-BUSY; fi
    return
  fi

  # Status-borne death outranks log-borne evidence: the status is a fact about
  # the process, the log text is a string somebody printed.
  if [ "$rc" -ge 3221225472 ] 2>/dev/null; then echo SIGNAL; return; fi
  if [ "$rc" -gt 128 ] && [ "$rc" -lt 193 ]; then echo SIGNAL; return; fi

  if grep -qaiE 'SIGSEGV|rust panic|fatal runtime error|access violation|EXCEPTION_ACCESS_VIOLATION|stack overflow' "$logf" 2>/dev/null; then
    echo CRASH; return
  fi
  if grep -qa '^CORPUS-NOMAIN ' "$logf" 2>/dev/null; then echo NOSTART; return; fi
  if ! grep -qa '^CORPUS-START ' "$logf" 2>/dev/null; then echo NOSTART; return; fi
  echo RAN
}

# The comparable key for a RAN arm. Only the wrapper's own markers and any
# SBRUNNER_RESULT line: everything else a real application prints is timestamps,
# temp paths, thread names and heap addresses, none of which two VMs can be
# expected to match and all of which would manufacture divergences.
#
# The stack trace after CORPUS-THROW is deliberately excluded from the key and
# kept in the log: the THROWN TYPE is the answer, the frame list is not.
#
# HOOK-SOURCED END LINES ARE DROPPED, and this one line is worth its comment.
# CorpusMain prints `CORPUS-END <c> completed=true` on the NORMAL return path
# (bytecode the workload's own VM executed -- a real signal, kept) and
# `CORPUS-END <c> completed=exit` from a SHUTDOWN HOOK, for the System.exit
# path. CratonVM runs no shutdown hooks in any mode, and every `junit`-kind
# workload exits through System.exit, so that line appears on HotSpot and never
# on CratonVM -- one known VM gap, present in every single junit row.
#
# Measured 2026-08-12 over the runs in ./out: 33 DIVERGE rows across five
# bc-java/commons-math runs come back AGREE once this line is dropped, with
# byte-identical SBRUNNER_RESULT counts and identical exit statuses on both
# arms; not one AGREE row flips the other way. In the worst run 11 of 13
# DIVERGE rows were this artefact, which did not merely inflate the count -- it
# BURIED THE TWO GENUINE DIVERGENCES AMONG ELEVEN FAKE ONES. A harness that
# cries wolf eleven times in thirteen is worse than one that reports nothing.
#
# An earlier fix stripped the ` completed=<x>` FIELD instead. That was
# measurably insufficient (the `CORPUS-END` marker itself still appeared on one
# side only, so the rows still diverged) and it was also wrong in direction: it
# erased the true/exit distinction, which is the only thing that tells a
# hook-sourced line from a bytecode-sourced one. The absence of the hook line
# is still reported -- as a NOTE on the row, see `end_marker_note` -- so the
# shutdown-hook gap stays visible without being counted as 33 divergences.
arm_key() {
  grep -aE '^(CORPUS-START|CORPUS-END|CORPUS-THROW|CORPUS-NOMAIN|SBRUNNER_RESULT) ' "$1" 2>/dev/null \
    | sed 's/\x1b\[[0-9;]*m//g' \
    | sed 's/\r$//' \
    | sed '/^CORPUS-END .* completed=exit$/d'
}

has_hook_end() { grep -qa '^CORPUS-END .* completed=exit' "$1" 2>/dev/null; }

end_marker_note() {
  local cvlog="$1" hslog="$2"
  if has_hook_end "$hslog" && ! has_hook_end "$cvlog"; then
    echo "[hook-END on HotSpot only: CratonVM runs no shutdown hooks -- excluded from the key, tracked separately]"
  fi
}

# A HotSpot arm that started nothing and asserted nothing is a DISAGREEING
# PRECONDITION, not a passing oracle: the workload declined to run on the
# oracle too, so there is no ground truth to compare against. This is the
# same defect regression-suite/harness-guard.sh:166 `harness_guard_oracle`
# guards for in the small-vector suite (its G4). Scoring it green is the
# single most expensive mistake available here, because it converts "we
# learned nothing" into "we verified it".
oracle_vacuous() {
  local logf="$1" line t a thr

  # (a) A junit-kind oracle that THREW and never printed an SBRUNNER_RESULT
  #     died during discovery. No test ran on the reference side, so there is
  #     nothing to be right or wrong about.
  if [ "${CORPUS_KIND:-main}" = junit ] \
     && grep -qa '^CORPUS-THROW ' "$logf" 2>/dev/null \
     && ! grep -qa '^SBRUNNER_RESULT ' "$logf" 2>/dev/null; then
    thr="$(grep -a '^CORPUS-THROW ' "$logf" | tail -1 | head -c 120)"
    echo "oracle threw before any SBRUNNER_RESULT ($thr) -- it died during DISCOVERY, so no test ran on the reference side"
    return 0
  fi

  # (b) A linkage-family throw escaping to the wrapper means the FIXTURE is
  #     broken on the oracle: a missing jar, a version-skewed JUnit stack, a
  #     class that will not initialise. Both arms then fail identically and
  #     the row scores AGREE -- "we learned nothing" rendered as "we verified
  #     it". This exact hole was written up in corpora.d/commons-math.sh: the
  #     platform launcher's ClasspathAlignmentChecker throws JUnitException on
  #     both arms and the harness called it agreement.
  if grep -qaE '^CORPUS-THROW .*(NoClassDefFoundError|ClassNotFoundException|UnsupportedClassVersionError|NoSuchMethodError|NoSuchFieldError|IncompatibleClassChangeError|ExceptionInInitializerError|JUnitException|LinkageError)' "$logf" 2>/dev/null; then
    thr="$(grep -aE '^CORPUS-THROW ' "$logf" | tail -1 | head -c 120)"
    echo "oracle died of a LINKAGE/fixture failure ($thr) -- the fixture is broken on the reference side; an identical failure on both arms is not agreement"
    return 0
  fi

  line="$(grep -a '^SBRUNNER_RESULT ' "$logf" 2>/dev/null | tail -1)"
  if [ -n "$line" ]; then
    case "$line" in
      *"tests=0"*) echo "SBRUNNER_RESULT reports tests=0 -- the oracle discovered and started nothing"; return 0 ;;
    esac
    # Every discovered test aborted: an `assumeTrue`/`Assumptions.abort` that
    # disagreed with the host. See the memory record on HotSpot oracles that
    # report ok=0 aborted=<all>.
    t="$(printf '%s' "$line" | sed -n 's/.*tests=\([0-9]*\).*/\1/p')"
    a="$(printf '%s' "$line" | sed -n 's/.*aborted=\([0-9]*\).*/\1/p')"
    if [ -n "$t" ] && [ -n "$a" ] && [ "$t" -gt 0 ] && [ "$a" -eq "$t" ]; then
      echo "SBRUNNER_RESULT reports every one of $t discovered tests ABORTED -- a disagreeing precondition on the oracle, not a pass"
      return 0
    fi
  fi
  return 1
}

# --- adjudication ------------------------------------------------------------
#
# One function, used by cmd_run AND by `selfcheck`, so the self-check exercises
# the code that actually produces verdicts rather than a paraphrase of it.
#
#   adjudicate <cvstate> <cvrc> <cvlog> <hsstate> <hsrc> <hslog> \
#              <cv_silent_s> <cv_ms> <hs_ms> <cap_s>
#
# Sets ADJ_VERDICT, ADJ_NOTE, ADJ_BUCKET (agree|diverge|broken|unadj|harness).
ADJ_VERDICT=""; ADJ_NOTE=""; ADJ_BUCKET=""
adjudicate() {
  local cvstate="$1" cvrc="$2" cvlog="$3" hsstate="$4" hsrc="$5" hslog="$6"
  local cvsilent="${7:--1}" cvms="${8:-0}" hsms="${9:-0}" cap="${10:-0}"
  local cvkey hskey ratio
  ADJ_VERDICT=""; ADJ_NOTE=""; ADJ_BUCKET=""

  # 0. A launcher fault on EITHER arm is about this script or this host, not
  #    about the VM, and it must not be counted as a CratonVM failure.
  if [ "$cvstate" = LAUNCH-FAILED ] || [ "$hsstate" = LAUNCH-FAILED ]; then
    ADJ_VERDICT=HARNESS-ERROR; ADJ_BUCKET=harness
    ADJ_NOTE="launcher never ran: cv=$(decode_status "$cvrc") hs=$(decode_status "$hsrc"). Fix the invocation; this row is not evidence about the VM."
    return
  fi

  if [ "$hsstate" = SKIPPED ]; then
    ADJ_VERDICT=UNADJUDICATED; ADJ_BUCKET=unadj
    ADJ_NOTE="--no-oracle: no ground truth was obtained"
    return
  fi

  if [ "$hsstate" != RAN ]; then
    # G4-equivalent. The 'expected' side is an artefact of the oracle's own
    # failure, so there is nothing to compare against and this must not be
    # scored either way.
    ADJ_VERDICT=ORACLE-UNUSABLE; ADJ_BUCKET=unadj
    ADJ_NOTE="HotSpot arm state=$hsstate $(decode_status "$hsrc") -- no ground truth; fix the workload/fixture before reading the CratonVM column"
    return
  fi

  if ADJ_NOTE="$(oracle_vacuous "$hslog")"; then
    ADJ_VERDICT=ORACLE-VACUOUS; ADJ_BUCKET=unadj
    return
  fi
  ADJ_NOTE=""

  if [ "$cvstate" != RAN ]; then
    ADJ_VERDICT="CV-$cvstate"; ADJ_BUCKET=broken
    case "$cvstate" in
      TIMEOUT-STALLED)
        ADJ_NOTE="killed at the ${cap}s cap after ${cvsilent}s with NO output: SILENT AT THE WALL. That is a hang/stall, not slowness -- on this VM it is very often a SIGSEGV or a livelock that printed no result line. $(decode_status "$cvrc")." ;;
      TIMEOUT-BUSY)
        ADJ_NOTE="killed at the ${cap}s cap while STILL WRITING output (last write ${cvsilent}s before the kill). This row does NOT separate 'hung' from 'slower than one cap' -- it was tried at exactly one cap. Re-run with --timeout $((cap*3)) before asserting either." ;;
      TIMEOUT-UNKNOWN)
        ADJ_NOTE="killed at the ${cap}s cap; the arm's output silence could not be measured, so hung vs slow is UNDETERMINED here." ;;
      SIGNAL)
        ADJ_NOTE="died of $(decode_status "$cvrc") -- a hard failure with a status, not a test result. $(grep -aiE 'SIGSEGV|rust panic|fatal runtime error|access violation|stack overflow' "$cvlog" 2>/dev/null | tail -1 | head -c 120)" ;;
      CRASH)
        ADJ_NOTE="$(grep -aiE 'SIGSEGV|rust panic|fatal runtime error|access violation|stack overflow' "$cvlog" 2>/dev/null | tail -1 | head -c 160)"
        [ -n "$ADJ_NOTE" ] || ADJ_NOTE="crash evidence in the log; $(decode_status "$cvrc")" ;;
      NOSTART)
        ADJ_NOTE="$(grep -a '^CORPUS-NOMAIN ' "$cvlog" 2>/dev/null | tail -1 | head -c 160)"
        [ -n "$ADJ_NOTE" ] || ADJ_NOTE="no CORPUS-START marker: the workload class never began executing (classpath or launcher problem, not a VM answer); $(decode_status "$cvrc")" ;;
      *)
        ADJ_NOTE="CratonVM arm state=$cvstate $(decode_status "$cvrc")" ;;
    esac
    # An order-of-magnitude BOUND against the oracle's own wall. Not a
    # throughput number and not comparable across runs -- it exists so that
    # "killed at 13x the oracle and silent for the last 9 minutes" cannot be
    # mistaken for "a bit slow".
    case "$cvstate" in
      TIMEOUT-*)
        if [ "$hsms" -gt 0 ] 2>/dev/null && [ "$cvms" -gt 0 ] 2>/dev/null; then
          ratio=$(( cvms / hsms ))
          ADJ_NOTE="$ADJ_NOTE Oracle finished the same workload in ${hsms} ms; CratonVM was killed at >=${ratio}x that wall (ORDER-OF-MAGNITUDE BOUND, not a measurement)."
        fi ;;
    esac
    return
  fi

  cvkey="$(arm_key "$cvlog")"
  hskey="$(arm_key "$hslog")"

  # The most dangerous failure this file can have: if the key extractor ever
  # matches nothing (a typo in the marker alternation, a renamed marker), every
  # comparison becomes "" = "" and the whole corpus reports AGREE. An empty key
  # from an arm that reached RAN is impossible by construction -- RAN requires a
  # CORPUS-START line, which is in the key -- so if it happens, the extractor is
  # broken and the row must not be scored.
  if [ -z "$cvkey" ] || [ -z "$hskey" ]; then
    ADJ_VERDICT=HARNESS-ERROR; ADJ_BUCKET=harness
    ADJ_NOTE="arm reached RAN but produced an EMPTY comparison key (cv=${#cvkey} hs=${#hskey} chars). arm_key is broken; do not read this row, and do not read any AGREE in this run."
    return
  fi

  if [ "$cvkey" = "$hskey" ]; then
    # Markers can agree while the process statuses disagree -- e.g. an arm that
    # exits non-zero after printing the same counts. Measured across every run
    # in ./out: no row that agrees on markers disagrees on exit status, so this
    # manufactures nothing today; it exists so that a future silent early exit
    # cannot hide behind a dropped hook line.
    if [ "$cvrc" != "$hsrc" ]; then
      ADJ_VERDICT=DIVERGE; ADJ_BUCKET=diverge
      ADJ_NOTE="markers agree but EXIT STATUS differs: cv=$(decode_status "$cvrc") hs=$(decode_status "$hsrc")"
      return
    fi
    ADJ_VERDICT=AGREE; ADJ_BUCKET=agree
    ADJ_NOTE="$(end_marker_note "$cvlog" "$hslog")"
    return
  fi

  ADJ_VERDICT=DIVERGE; ADJ_BUCKET=diverge
  ADJ_NOTE="markers differ from HotSpot $(end_marker_note "$cvlog" "$hslog")"
}

# --- subcommands ------------------------------------------------------------

cmd_list() {
  local def name
  printf '%-18s %-11s %-6s %s\n' CORPUS CONFIDENCE STATE ROOT
  for def in "$CORPORA_D"/*.sh; do
    [ -e "$def" ] || continue
    name="$(basename "$def" .sh)"
    ( load_corpus "$name"
      if resolve_root; then
        printf '%-18s %-11s %-6s %s\n' "$name" "$CORPUS_CONFIDENCE" BUILT "$CORPUS_ROOT"
      else
        printf '%-18s %-11s %-6s %s\n' "$name" "$CORPUS_CONFIDENCE" "-" "(no built root among candidates)"
      fi )
  done
}

cmd_info() {
  local name="${1:-}"; [ -n "$name" ] || usage_die "info needs a corpus name"
  load_corpus "$name"
  echo "corpus:      $CORPUS"
  echo "description: $CORPUS_DESC"
  echo "kind:        $CORPUS_KIND"
  echo "confidence:  $CORPUS_CONFIDENCE"
  [ -n "$CORPUS_NOTE" ] && echo "note:        $CORPUS_NOTE"
  [ -n "$CORPUS_CLEAN_PATHS" ] && echo "clean paths: $CORPUS_CLEAN_PATHS (removed before EACH arm)"
  root_report
  if resolve_root; then
    echo "  resolved root: $CORPUS_ROOT"
    cp_reset
    if ! corpus_classpath "$CORPUS_ROOT"; then
      echo "  classpath: REFUSED (see the error above). This corpus cannot be run until its fixture is repaired."
      return 2
    fi
    echo "  classpath entries: $(cp_count)   characters: ${#CP}"
    if [ "${#CP}" -gt 30000 ]; then
      echo "  NOTE: ${#CP} chars exceeds a Windows command line; the driver puts it in an @argfile, which is why that is not fatal."
    fi
    echo "  default workload class: ${CORPUS_DEFAULT_CLASS:-<none declared>}"
  else
    echo "  resolved root: NONE -- this corpus cannot be run on this host as configured."
    return 2
  fi
}

cmd_discover() {
  local name="${1:-}"; shift || true
  [ -n "$name" ] || usage_die "discover needs a corpus name"
  while [ $# -gt 0 ]; do
    case "$1" in
      --limit) LIMIT="${2:-0}"; shift 2 ;;
      *) usage_die "unknown discover option '$1'" ;;
    esac
  done
  load_corpus "$name"
  resolve_root || die "corpus '$name' has no BUILT root on this host; run 'info $name' to see the candidates"
  if [ "$LIMIT" -gt 0 ]; then
    # `set -o pipefail` + `head` closing the pipe early would report a
    # SUCCESSFUL discovery as a failed one, because the producer dies of
    # SIGPIPE. Buffer instead of piping.
    corpus_discover "$CORPUS_ROOT" > "${TMPDIR:-/tmp}/corpus-discover.$$" || true
    head -n "$LIMIT" "${TMPDIR:-/tmp}/corpus-discover.$$"
    rm -f "${TMPDIR:-/tmp}/corpus-discover.$$"
  else
    corpus_discover "$CORPUS_ROOT"
  fi
}

# Normalise the workload list: strip CR (this repo is checked out with
# core.autocrlf=true, and a trailing \r inside a class name produces a NOSTART
# row that reads as a launcher defect), strip surrounding whitespace, drop
# comments and blanks, and REFUSE anything that is not shaped like an FQCN.
normalize_classes() {
  printf '%s\n' "$1" \
    | sed 's/\r$//; s/^[[:space:]]*//; s/[[:space:]]*$//' \
    | grep -av '^#' \
    | grep -a .
}

cmd_run() {
  local name="${1:-}"; shift || true
  [ -n "$name" ] || usage_die "run needs a corpus name"
  while [ $# -gt 0 ]; do
    case "$1" in
      --class)        WORKCLASS="${2:-}"; shift 2 ;;
      --classes-from) CLASSES_FROM="${2:-}"; shift 2 ;;
      --mode)         MODE="${2:-}"; shift 2 ;;
      --cv)           CV="${2:-}"; shift 2 ;;
      --jdk)          JDK="${2:-}"; shift 2 ;;
      --timeout)      TIMEOUT="${2:-}"; shift 2 ;;
      --out)          OUT="${2:-}"; shift 2 ;;
      --cv-args)      CV_ARGS="${2:-}"; shift 2 ;;
      --no-oracle)    NO_ORACLE=1; shift ;;
      *) usage_die "unknown run option '$1'" ;;
    esac
  done

  # A non-numeric --timeout does not fail as "bad option": `timeout abc ...`
  # exits 125 for every workload, which used to classify as NOSTART, i.e. as a
  # corpus problem. Refuse it here instead.
  case "$TIMEOUT" in
    ''|*[!0-9]*) usage_die "--timeout must be a whole number of seconds, got '$TIMEOUT'" ;;
  esac
  [ "$TIMEOUT" -gt 0 ] || usage_die "--timeout must be > 0"

  load_corpus "$name"
  resolve_tools
  local mflags; mflags="$(mode_flags)"

  [ -n "$CV" ] || die "no CratonVM binary: pass --cv, or build \$ROOT/target/release/cratonvm.exe (this script never builds it)"
  [ -x "$CV" ] || die "CratonVM binary is not executable: $CV"

  if ! resolve_root; then
    echo "corpus '$name' has no BUILT root on this host." >&2
    root_report >&2
    die "nothing to run -- this is a fixture precondition failure, not a VM result"
  fi

  # The workload list.
  local classes=""
  if [ -n "$CLASSES_FROM" ]; then
    [ -s "$CLASSES_FROM" ] || die "--classes-from '$CLASSES_FROM' is missing or empty"
    classes="$(normalize_classes "$(cat "$CLASSES_FROM")")"
  elif [ -n "$WORKCLASS" ]; then
    classes="$(normalize_classes "$WORKCLASS")"
  elif [ -n "$CORPUS_DEFAULT_CLASS" ]; then
    classes="$(normalize_classes "$CORPUS_DEFAULT_CLASS")"
  else
    die "corpus '$name' declares no default workload; pass --class or --classes-from"
  fi

  # A run with nothing to run used to print `AGREE=0 DIVERGE=0 CV-BROKEN=0
  # UNADJUDICATED=0` and exit 0 -- a green result for zero evidence. Measured
  # 2026-08-12 with a --classes-from file containing only comments. Refuse.
  local nclasses; nclasses="$(printf '%s\n' "$classes" | grep -c . || true)"
  [ "${nclasses:-0}" -gt 0 ] || die "the effective workload list is EMPTY (after dropping comments and blanks). A run that adjudicates nothing must not exit 0."
  local c
  for c in $classes; do
    case "$c" in
      *[!A-Za-z0-9_.\$]*|.*|*.) die "workload entry '$c' is not shaped like a fully-qualified class name. A mangled entry produces a NOSTART row on both arms that reads as a launcher defect." ;;
    esac
  done

  # Compose the corpus classpath FIRST: a JUnit runner source needs it to
  # compile against, and the emptiness check below must not be satisfied by
  # the wrapper's own build directory.
  cp_reset
  corpus_classpath "$CORPUS_ROOT" || die "corpus '$name' failed to compose a classpath from $CORPUS_ROOT"
  [ -n "$CP" ] || die "composed classpath is EMPTY for '$name' at $CORPUS_ROOT"
  ensure_wrapper "$CP"
  cp_add "$BUILD"                      # CorpusMain, and the JUnit runner if any

  mkdir -p "$OUT" || die "cannot create --out directory $OUT"
  OUT="$(native_dir "$OUT")"           # see native_dir: a /c/... @argfile is unopenable
  local stamp; stamp="$(date +%Y%m%d-%H%M%S)"
  local rundir="$OUT/$name-$MODE-$stamp"
  mkdir -p "$rundir" || die "cannot create run directory $rundir"
  local tsv="$rundir/results.tsv"
  local argf="$rundir/cp.args"
  write_argfile "$argf" "$CP"

  # Provenance, and the remedy for a hazard this file has already produced
  # twice: it was observed TRANSIENTLY UNPARSEABLE while another session had it
  # open, because bash reads a script incrementally and a rewrite under a
  # running interpreter changes what it executes next. The snapshot records
  # exactly which text produced this TSV.
  cp -- "${BASH_SOURCE[0]}" "$rundir/run-corpus.sh.snapshot" 2>/dev/null || true
  local selfsha; selfsha="$(sha256sum "${BASH_SOURCE[0]}" 2>/dev/null | cut -c1-16)"

  local wd; wd="$(corpus_workdir "$CORPUS_ROOT")"
  # A cd that fails inside the arm subshell would make the arm exit 1 without
  # running anything, which classifies as NOSTART and reads as a classpath
  # defect. Check it once, loudly, here.
  [ -n "$wd" ] || die "corpus '$name' resolved an EMPTY working directory"
  [ -d "$wd" ] || die "corpus '$name' working directory does not exist: $wd"

  preflight_argfile "$argf" "$rundir/oracle-version.txt"

  {
    echo "# corpus=$name root=$CORPUS_ROOT mode=$MODE"
    echo "# cv=$CV"
    echo "# jdk=$JDK"
    echo "# driver=run-corpus.sh sha256:${selfsha:-unknown} (snapshot in this directory)"
    echo "# oracle=$(head -1 "$rundir/oracle-version.txt" 2>/dev/null)"
    echo "# workdir=$wd  clean_before_each_arm='${CORPUS_CLEAN_PATHS:-<none>}'"
    echo "# arm order: CratonVM first, then HotSpot, in the SAME working directory."
    echo "# timeout=${TIMEOUT}s  cp_entries=$(cp_count)  cp_chars=${#CP}  workloads=$nclasses"
    echo "# ms columns are NON-METRIC: this host's wall clock does not support throughput claims."
    echo "# *_silent_s is seconds between an arm's last output and its kill; it is the only"
    echo "#   evidence separating a hung arm from one that was merely slower than the cap."
    printf 'class\tverdict\tcv_state\tcv_rc\tcv_status\tcv_ms\tcv_silent_s\ths_state\ths_rc\ths_status\ths_ms\tnote\n'
  } > "$tsv"

  # `main`-kind workloads are invoked directly through the wrapper.
  # `junit`-kind workloads have no main of their own, so the wrapper invokes
  # the corpus's JUnit-platform runner and passes the test class to IT. The
  # wrapper's start/end/throw markers therefore mean the same thing in both
  # kinds, and the runner's own SBRUNNER_RESULT line adds the per-test counts.
  local invoke_prefix=""
  if [ "$CORPUS_KIND" = junit ]; then
    [ -n "$CORPUS_RUNNER_CLASS" ] || die "corpus '$name' is kind=junit but declares no CORPUS_RUNNER_CLASS"
    invoke_prefix="$CORPUS_RUNNER_CLASS"
  fi

  local agree=0 diverge=0 broken=0 unadj=0 harness=0 rows=0
  for c in $classes; do
    local safe; safe="$(printf '%s' "$c" | sed 's/[^A-Za-z0-9_.-]/_/g')"
    local cvlog="$rundir/$safe.cv.log" hslog="$rundir/$safe.hs.log"
    local t0 t1 cvrc hsrc cvms hsms cvstate hsstate cvsilent hssilent

    # ---- CratonVM arm ----
    # The VM's own watchdog is disabled so that `timeout` is the single
    # authority on the wall; two independent killers make a TIMEOUT row
    # impossible to attribute. regression-suite/run.sh:443 does the same.
    # Run in the corpus's OWN working directory. `$wd` was once computed and
    # never used, so both arms ran in the invoking cwd -- measured
    # consequences, both of which read as VM findings and were not:
    #   * H2 wrote its databases into the git worktree as untracked `data/`,
    #     and a carried-over corrupt store made `TestBackup` DIVERGE where all
    #     three arms are green when run alone.
    #   * Running Tomcat's `TestSsl` from the right root took the HOTSPOT
    #     ORACLE from failed=7 to failed=1 -- six real oracle passes were being
    #     scored as failures, i.e. the harness was manufacturing divergence on
    #     the reference side.
    # A subshell keeps the cd local; every other path here is absolute.
    # State the corpus declares as per-run scratch is removed FIRST, so the
    # second arm never inherits the first arm's database files.
    clean_arm_state "$wd"
    t0=$(date +%s%3N)
    ( cd "$wd" && CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 timeout "$TIMEOUT" \
      "$CV" --java-home "$JDK" $mflags $CV_ARGS "@$argf" CorpusMain $invoke_prefix "$c" ) \
      > "$cvlog" 2>&1
    cvrc=$?
    t1=$(date +%s%3N); cvms=$((t1-t0))
    cvsilent="$(log_silence_secs "$cvlog" "$((t1/1000))")"
    cvstate="$(classify_arm "$cvrc" "$cvlog" "$TIMEOUT" "$((t1/1000))")"

    # ---- HotSpot oracle arm ----
    # The oracle NEVER receives $CV_ARGS or the mode flag: those are CratonVM
    # spellings and the oracle has to stay the plain reference run. And the
    # oracle is HotSpot, never the other CratonVM mode -- a
    # CratonVM-vs-CratonVM comparison proves nothing about correctness,
    # only about self-consistency.
    if [ "$NO_ORACLE" -eq 1 ]; then
      hsstate=SKIPPED; hsrc=-1; hsms=0; hssilent=-1
      : > "$hslog"
    else
      # Same working directory as the CratonVM arm -- see the note there. An
      # oracle run from the wrong cwd manufactures divergence on the REFERENCE
      # side, which is the worst possible place for it: a red oracle row reads
      # as a VM defect and is scored as one.
      clean_arm_state "$wd"
      t0=$(date +%s%3N)
      ( cd "$wd" && timeout "$TIMEOUT" "$HS" "@$argf" CorpusMain $invoke_prefix "$c" ) \
        > "$hslog" 2>&1
      hsrc=$?
      t1=$(date +%s%3N); hsms=$((t1-t0))
      hssilent="$(log_silence_secs "$hslog" "$((t1/1000))")"
      hsstate="$(classify_arm "$hsrc" "$hslog" "$TIMEOUT" "$((t1/1000))")"
    fi

    # ---- adjudication ----
    adjudicate "$cvstate" "$cvrc" "$cvlog" "$hsstate" "$hsrc" "$hslog" \
               "$cvsilent" "$cvms" "$hsms" "$TIMEOUT"
    case "$ADJ_BUCKET" in
      agree)   agree=$((agree+1)) ;;
      diverge) diverge=$((diverge+1))
               { echo "--- HotSpot ---"; arm_key "$hslog"
                 echo "--- CratonVM ---"; arm_key "$cvlog"
                 echo "--- status ---"; echo "cv $(decode_status "$cvrc")  hs $(decode_status "$hsrc")"
               } > "$rundir/$safe.diff" ;;
      broken)  broken=$((broken+1)) ;;
      unadj)   unadj=$((unadj+1)) ;;
      harness) harness=$((harness+1)) ;;
    esac
    rows=$((rows+1))

    # Tabs and newlines in a note would silently add columns/rows to the TSV.
    local note; note="$(printf '%s' "$ADJ_NOTE" | tr '\t\n' '  ')"
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
      "$c" "$ADJ_VERDICT" "$cvstate" "$cvrc" "$(decode_status "$cvrc")" "$cvms" "$cvsilent" \
      "$hsstate" "$hsrc" "$(decode_status "$hsrc")" "$hsms" "$note" >> "$tsv"
    printf '  %-52s %-18s cv=%-16s hs=%-8s %s\n' "$c" "$ADJ_VERDICT" "$cvstate" "$hsstate" "$note"
  done

  echo
  echo "corpus=$name mode=$MODE  AGREE=$agree DIVERGE=$diverge CV-BROKEN=$broken UNADJUDICATED=$unadj HARNESS-ERROR=$harness"
  echo "results: $tsv"

  # "We learned nothing" must never render as "we verified it".
  if [ "$rows" -eq 0 ]; then
    echo "NO WORKLOAD RAN. This run is not evidence about the VM." >&2
    return 2
  fi
  if [ "$harness" -gt 0 ]; then
    echo "HARNESS-ERROR on $harness row(s): the driver or its inputs failed, not the VM. Fix that first; the rest of this run is suspect." >&2
    return 2
  fi
  if [ "$agree" -eq 0 ] && [ "$diverge" -eq 0 ] && [ "$broken" -eq 0 ]; then
    echo "NOTHING WAS ADJUDICATED. This run is not evidence about the VM." >&2
    return 2
  fi
  [ "$diverge" -eq 0 ] && [ "$broken" -eq 0 ] && return 0
  return 1
}

# --- selfcheck ---------------------------------------------------------------
#
# The standing positive control. `scripts/check-no-diag-prints.sh` is the
# precedent: it was a BLOCKING gate whose search ended in `|| true`, so "clean
# tree" and "the search failed" were the same output, and it was fixed by
# adding a sentinel pattern that MUST match. The corpus driver has the same
# exposure in a worse place -- a comparison that cannot go red reports a green
# corpus -- so it carries its own control.
#
# Part 1 (unit) drives classify_arm / arm_key / oracle_vacuous / adjudicate
# over synthetic logs. Part 2 (end-to-end) runs cmd_run itself, twice, against
# a throwaway corpus with REAL HotSpot as the oracle and a stub standing in for
# cratonvm.exe: once with a stub that agrees (must exit 0, AGREE=1) and once
# with a stub that does not (must exit 1, DIVERGE=1). Part 2 is what makes this
# more than a paraphrase: a mutation test only tests something if the mutated
# branch actually executes, and this one executes the same cmd_run that
# produces every corpus result.
SC_FAIL=0
sc_assert() {
  if [ "$2" = "$3" ]; then
    printf '  ok    %-56s = %s\n' "$1" "$3"
  else
    printf '  FAIL  %-56s expected=%s actual=%s\n' "$1" "$2" "$3"
    SC_FAIL=$((SC_FAIL+1))
  fi
}

sc_mklog() { printf '%s\n' "$2" > "$1"; }

selfcheck_unit() {
  local T="$1"
  local now; now=$(date +%s)
  echo "-- unit: classification"

  sc_mklog "$T/ran.log" "CORPUS-START X
CORPUS-END X completed=true"
  sc_assert "classify RAN"              RAN             "$(classify_arm 0 "$T/ran.log" 600 "$now")"

  sc_mklog "$T/nostart.log" "some application noise"
  sc_assert "classify NOSTART"          NOSTART         "$(classify_arm 1 "$T/nostart.log" 600 "$now")"

  sc_mklog "$T/panic.log" "thread 'main' panicked: rust panic here"
  sc_assert "classify CRASH (log text)" CRASH           "$(classify_arm 101 "$T/panic.log" 600 "$now")"

  sc_assert "classify SIGNAL (139)"     SIGNAL          "$(classify_arm 139 "$T/nostart.log" 600 "$now")"
  sc_assert "classify SIGNAL (NTSTATUS)" SIGNAL         "$(classify_arm 3221225477 "$T/nostart.log" 600 "$now")"
  sc_assert "classify SIGNAL (137 is not a timeout)" SIGNAL "$(classify_arm 137 "$T/nostart.log" 600 "$now")"

  : > "$T/empty.log"
  sc_assert "classify LAUNCH-FAILED (127)" LAUNCH-FAILED "$(classify_arm 127 "$T/empty.log" 600 "$now")"
  sc_assert "classify LAUNCH-FAILED (125)" LAUNCH-FAILED "$(classify_arm 125 "$T/empty.log" 600 "$now")"

  # The wall, split. Same rc=124, same log; only the silence differs.
  sc_mklog "$T/tmo.log" "CORPUS-START X"
  sc_assert "classify TIMEOUT-BUSY (writing at the wall)" TIMEOUT-BUSY \
    "$(classify_arm 124 "$T/tmo.log" 600 "$now")"
  sc_assert "classify TIMEOUT-STALLED (silent at the wall)" TIMEOUT-STALLED \
    "$(classify_arm 124 "$T/tmo.log" 600 "$((now+600))")"
  sc_assert "classify TIMEOUT-UNKNOWN (no such log)" TIMEOUT-UNKNOWN \
    "$(classify_arm 124 "$T/does-not-exist.log" 600 "$now")"

  echo "-- unit: comparison key"
  # HotSpot prints the hook-sourced END; CratonVM cannot. Same counts.
  sc_mklog "$T/hs1.log" "CORPUS-START SbRunner
SBRUNNER_RESULT tests=26 failed=0 aborted=0 skipped=0 containersFailed=0
CORPUS-END SbRunner completed=exit"
  sc_mklog "$T/cv1.log" "CORPUS-START SbRunner
SBRUNNER_RESULT tests=26 failed=0 aborted=0 skipped=0 containersFailed=0"
  adjudicate RAN 0 "$T/cv1.log" RAN 0 "$T/hs1.log" 0 10 10 600
  sc_assert "hook-sourced CORPUS-END is not a divergence" AGREE "$ADJ_VERDICT"
  case "$ADJ_NOTE" in
    *hook-END*) sc_assert "the hook gap is still REPORTED on the row" yes yes ;;
    *)          sc_assert "the hook gap is still REPORTED on the row" yes no ;;
  esac

  # THE SENTINEL. If this ever says AGREE, the comparison machinery cannot go
  # red and every green result in this driver is worthless.
  sc_mklog "$T/cv2.log" "CORPUS-START SbRunner
SBRUNNER_RESULT tests=26 failed=3 aborted=0 skipped=0 containersFailed=0"
  adjudicate RAN 0 "$T/cv2.log" RAN 0 "$T/hs1.log" 0 10 10 600
  sc_assert "SENTINEL: a real count difference DIVERGES" DIVERGE "$ADJ_VERDICT"

  # The bytecode-sourced END is a real signal and must NOT be normalised away.
  sc_mklog "$T/hs3.log" "CORPUS-START X
CORPUS-END X completed=true"
  sc_mklog "$T/cv3.log" "CORPUS-START X"
  adjudicate RAN 0 "$T/cv3.log" RAN 0 "$T/hs3.log" 0 10 10 600
  sc_assert "a MISSING normal-path CORPUS-END diverges" DIVERGE "$ADJ_VERDICT"

  # Markers equal, statuses not.
  adjudicate RAN 1 "$T/ran.log" RAN 0 "$T/ran.log" 0 10 10 600
  sc_assert "equal markers + unequal exit status diverges" DIVERGE "$ADJ_VERDICT"

  # An empty key must never be scored as agreement.
  sc_mklog "$T/nomarkers.log" "nothing here"
  adjudicate RAN 0 "$T/nomarkers.log" RAN 0 "$T/nomarkers.log" 0 10 10 600
  sc_assert "empty comparison key is a HARNESS-ERROR, not AGREE" HARNESS-ERROR "$ADJ_VERDICT"

  echo "-- unit: oracle guards"
  local saved_kind="${CORPUS_KIND:-main}"
  CORPUS_KIND=junit
  sc_mklog "$T/vac0.log" "CORPUS-START SbRunner
SBRUNNER_RESULT tests=0 failed=0 aborted=0 skipped=0 containersFailed=0"
  sc_assert "oracle tests=0 is vacuous" yes "$(oracle_vacuous "$T/vac0.log" >/dev/null && echo yes || echo no)"
  sc_mklog "$T/vac1.log" "CORPUS-START SbRunner
SBRUNNER_RESULT tests=7 failed=0 aborted=7 skipped=0 containersFailed=0"
  sc_assert "oracle all-aborted is vacuous" yes "$(oracle_vacuous "$T/vac1.log" >/dev/null && echo yes || echo no)"
  sc_mklog "$T/vac2.log" "CORPUS-START SbRunner
CORPUS-THROW SbRunner org.junit.platform.commons.JUnitException"
  sc_assert "oracle discovery-time throw is vacuous" yes "$(oracle_vacuous "$T/vac2.log" >/dev/null && echo yes || echo no)"
  sc_mklog "$T/vac3.log" "CORPUS-START X
CORPUS-THROW X java.lang.NoClassDefFoundError"
  sc_assert "oracle linkage throw is vacuous" yes "$(oracle_vacuous "$T/vac3.log" >/dev/null && echo yes || echo no)"
  sc_assert "a healthy oracle is NOT vacuous" no "$(oracle_vacuous "$T/hs1.log" >/dev/null && echo yes || echo no)"
  # And the vacuity guard must beat an identical failure on both arms, which
  # would otherwise score AGREE -- the hole documented in corpora.d/commons-math.sh.
  adjudicate RAN 1 "$T/vac2.log" RAN 1 "$T/vac2.log" 0 10 10 600
  sc_assert "identical fixture failure on both arms is NOT agreement" ORACLE-VACUOUS "$ADJ_VERDICT"
  CORPUS_KIND="$saved_kind"

  echo "-- unit: status decoding"
  sc_assert "decode 124"  "timeout=124(expired-at-wall)"       "$(decode_status 124)"
  sc_assert "decode 127"  "harness=127(command-not-found)"     "$(decode_status 127)"
  sc_assert "decode 139"  "signal=11(SIGSEGV)"                 "$(decode_status 139)"
  sc_assert "decode 134"  "signal=6(SIGABRT)"                  "$(decode_status 134)"
  sc_assert "decode 0xC0000005" "ntstatus=0xC0000005(ACCESS_VIOLATION)" "$(decode_status 3221225477)"

  echo "-- unit: launcher fault is not a VM verdict"
  adjudicate LAUNCH-FAILED 127 "$T/empty.log" RAN 0 "$T/hs1.log" 0 1 1 600
  sc_assert "LAUNCH-FAILED -> HARNESS-ERROR" HARNESS-ERROR "$ADJ_VERDICT"

  echo "-- unit: timeout notes"
  adjudicate TIMEOUT-STALLED 124 "$T/tmo.log" RAN 0 "$T/hs1.log" 552 1509793 112531 1500
  case "$ADJ_NOTE" in
    *"SILENT AT THE WALL"*) sc_assert "stalled note says silent, not slow" yes yes ;;
    *) sc_assert "stalled note says silent, not slow" yes no ;;
  esac
  case "$ADJ_NOTE" in
    *">=13x"*) sc_assert "stalled note bounds the oracle ratio" yes yes ;;
    *) sc_assert "stalled note bounds the oracle ratio" yes no ;;
  esac
  adjudicate TIMEOUT-BUSY 124 "$T/tmo.log" RAN 0 "$T/hs1.log" 0 420000 112531 420
  case "$ADJ_NOTE" in
    *"exactly one cap"*) sc_assert "busy note refuses to assert hung-vs-slow" yes yes ;;
    *) sc_assert "busy note refuses to assert hung-vs-slow" yes no ;;
  esac
}

# Build a throwaway corpus, then run the REAL cmd_run against it twice.
selfcheck_e2e() {
  local T="$1"
  echo "-- end-to-end: the driver's own comparison, with HotSpot as the oracle"
  mkdir -p "$T/corpora.d" "$T/root/classes" "$T/build"

  cat > "$T/SelfCheckWorkload.java" <<'JAVA'
public final class SelfCheckWorkload {
    public static void main(String[] args) {
        System.out.println("SELFCHECK-WORKLOAD args=" + args.length);
        for (String a : args) {
            if ("boom".equals(a)) {
                throw new IllegalStateException("selfcheck-divergence");
            }
        }
    }
    private SelfCheckWorkload() { }
}
JAVA
  "$JAVAC" -d "$T/root/classes" "$T/SelfCheckWorkload.java" \
    || { echo "  FAIL  selfcheck workload did not compile"; SC_FAIL=$((SC_FAIL+1)); return; }

  cat > "$T/corpora.d/selfcheck.sh" <<SHELL
CORPUS_DESC="throwaway corpus for run-corpus.sh selfcheck"
CORPUS_KIND=main
CORPUS_CONFIDENCE=verified
CORPUS_ROOT_CANDIDATES="$T/root"
CORPUS_DEFAULT_CLASS="SelfCheckWorkload"
corpus_is_built() { [ -f "\$1/classes/SelfCheckWorkload.class" ]; }
corpus_classpath() { cp_add "\$1/classes"; return 0; }
corpus_discover() { echo SelfCheckWorkload; }
corpus_workdir() { echo "\$1"; }
SHELL

  # Two stubs standing in for cratonvm.exe. Both forward to the REAL java after
  # dropping the CratonVM-only flags, so the "VM" arm genuinely executes the
  # workload and the only difference is the injected argument. The oracle is
  # untouched HotSpot in both runs.
  local stub
  for stub in agree diverge; do
    {
      echo '#!/usr/bin/env bash'
      echo '# generated by run-corpus.sh selfcheck'
      echo 'args=()'
      echo 'while [ $# -gt 0 ]; do'
      echo '  case "$1" in'
      echo '    --java-home) shift 2 ;;'
      echo '    --real-jdk|--jdk-only|--synthetic-jdk) shift ;;'
      echo '    *) args+=("$1"); shift ;;'
      echo '  esac'
      echo 'done'
      if [ "$stub" = diverge ]; then
        echo "exec \"$HS\" \"\${args[@]}\" boom"
      else
        echo "exec \"$HS\" \"\${args[@]}\""
      fi
    } > "$T/stub-$stub.sh"
    chmod +x "$T/stub-$stub.sh"
  done

  local out rc
  out="$( CORPORA_D="$T/corpora.d" BUILD="$T/build" \
          bash "${BASH_SOURCE[0]}" run selfcheck --mode default \
            --cv "$T/stub-agree.sh" --out "$T/out-agree" --timeout 120 2>&1 )"
  rc=$?
  printf '%s\n' "$out" | sed 's/^/     | /'
  sc_assert "e2e: agreeing stub exits 0" 0 "$rc"
  sc_assert "e2e: agreeing stub scores AGREE=1" 1 "$(printf '%s' "$out" | sed -n 's/.*AGREE=\([0-9]*\).*/\1/p' | tail -1)"

  out="$( CORPORA_D="$T/corpora.d" BUILD="$T/build" \
          bash "${BASH_SOURCE[0]}" run selfcheck --mode default \
            --cv "$T/stub-diverge.sh" --out "$T/out-diverge" --timeout 120 2>&1 )"
  rc=$?
  printf '%s\n' "$out" | sed 's/^/     | /'
  sc_assert "e2e SENTINEL: diverging stub exits 1" 1 "$rc"
  sc_assert "e2e SENTINEL: diverging stub scores DIVERGE=1" 1 "$(printf '%s' "$out" | sed -n 's/.*DIVERGE=\([0-9]*\).*/\1/p' | tail -1)"

  # A workload list that is empty after comment-stripping must be refused, not
  # scored 0/0/0/0 and exited 0.
  printf '# nothing but a comment\n' > "$T/empty-classes.txt"
  out="$( CORPORA_D="$T/corpora.d" BUILD="$T/build" \
          bash "${BASH_SOURCE[0]}" run selfcheck --mode default \
            --cv "$T/stub-agree.sh" --out "$T/out-empty" --timeout 120 \
            --classes-from "$T/empty-classes.txt" 2>&1 )"
  rc=$?
  sc_assert "e2e: an empty workload list is refused (exit 2)" 2 "$rc"

  # A non-numeric --timeout is a usage error, not 125-for-every-workload.
  out="$( CORPORA_D="$T/corpora.d" BUILD="$T/build" \
          bash "${BASH_SOURCE[0]}" run selfcheck --mode default \
            --cv "$T/stub-agree.sh" --out "$T/out-badto" --timeout abc 2>&1 )"
  rc=$?
  sc_assert "e2e: a non-numeric --timeout is a usage error (exit 3)" 3 "$rc"
}

cmd_selfcheck() {
  local unit_only=0
  while [ $# -gt 0 ]; do
    case "$1" in
      --unit-only) unit_only=1; shift ;;
      *) usage_die "unknown selfcheck option '$1'" ;;
    esac
  done
  local T; T="$(mktemp -d 2>/dev/null || echo "${TMPDIR:-/tmp}/corpus-selfcheck.$$")"
  mkdir -p "$T"
  T="$(native_dir "$T")"
  echo "run-corpus.sh selfcheck   scratch=$T"
  selfcheck_unit "$T"
  if [ "$unit_only" -eq 0 ]; then
    resolve_tools
    selfcheck_e2e "$T"
  else
    echo "-- end-to-end: SKIPPED (--unit-only). The unit half cannot prove the"
    echo "   driver as a whole can go red; run without --unit-only before"
    echo "   trusting a green corpus."
  fi
  echo
  if [ "$SC_FAIL" -eq 0 ]; then
    echo "SELFCHECK PASS -- the comparison machinery can still report a divergence."
    rm -rf -- "$T" 2>/dev/null || true
    return 0
  fi
  echo "SELFCHECK FAILED: $SC_FAIL assertion(s). Do not trust any corpus result from this driver until they pass." >&2
  echo "scratch kept at $T" >&2
  return 1
}

# --- entry point ------------------------------------------------------------
#
# Everything above is a function definition and the only executable statement
# is `main "$@"` on the last line. bash reads a script incrementally, so a file
# rewritten under a running interpreter changes what that interpreter executes
# next -- this file was twice observed transiently unparseable while another
# session had it open, at plausible-looking line numbers. Keeping the tail
# minimal, and snapshotting the driver into every run directory, is what makes
# such an event visible instead of mysterious. Do not edit this file while a
# run is in flight.
main() {
  local sub="${1:-}"; shift || true
  case "$sub" in
    list)      cmd_list "$@" ;;
    info)      cmd_info "$@" ;;
    discover)  cmd_discover "$@" ;;
    run)       cmd_run "$@" ;;
    selfcheck) cmd_selfcheck "$@" ;;
    ""|-h|--help|help)
      sed -n '2,58p' "$HERE/run-corpus.sh" | sed 's/^# \{0,1\}//'
      exit 3 ;;
    *) usage_die "unknown subcommand '$sub'" ;;
  esac
}

main "$@"
