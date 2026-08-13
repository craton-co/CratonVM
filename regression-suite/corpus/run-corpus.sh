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
# Usage:
#   bash run-corpus.sh list
#   bash run-corpus.sh info    <corpus>
#   bash run-corpus.sh discover <corpus> [--limit N]
#   bash run-corpus.sh run     <corpus> [options]
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
#   0  every workload adjudicated AGREE
#   1  at least one workload DIVERGE, or the CratonVM arm failed
#   2  precondition failure (corpus missing/not built, no VM, no JDK,
#      oracle unusable or vacuous) -- nothing was adjudicated
#   3  usage error
###############################################################################
set -u
set -o pipefail

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
CORPORA_D="$HERE/corpora.d"
BUILD="$HERE/build"
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

# --- classification ---------------------------------------------------------
#
# Four outcomes, never merged:
#
#   NOSTART  the workload class never began executing (no CORPUS-START).
#            A classpath or launcher problem, NOT a VM answer.
#   CRASH    the VM died abnormally (SIGSEGV / rust panic / fatal).
#   TIMEOUT  killed at the wall. On this VM a timeout is VERY OFTEN a SIGSEGV
#            that produced no result line, not slowness -- so it is reported
#            as an unadjudicated hard failure and never as "slow".
#   RAN      the workload started and reached a terminal marker.
#
# `timeout` reports 124 on expiry. A Windows access violation surfaces as
# 3221225477 (0xC0000005); rc >= 128 is the POSIX signal form of the same idea.
classify_arm() {
  local rc="$1" logf="$2"
  if [ "$rc" -eq 124 ] || [ "$rc" -eq 137 ]; then echo TIMEOUT; return; fi
  if grep -qaiE 'SIGSEGV|rust panic|fatal runtime error|access violation|EXCEPTION_ACCESS_VIOLATION|stack overflow' "$logf" 2>/dev/null; then
    echo CRASH; return
  fi
  if [ "$rc" -eq 3221225477 ] || [ "$rc" -ge 128 ]; then echo CRASH; return; fi
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
arm_key() {
  # `completed=` is STRIPPED. It is printed from a shutdown hook, and CratonVM
  # never runs shutdown hooks in ANY mode -- measured 2026-08-12 in Spring's own
  # terms: a context calling registerShutdownHook() never destroys its beans.
  # So every `junit`-kind row diverged on that one line while its
  # SBRUNNER_RESULT counts were byte-identical: three independent lanes each
  # reported a sweeping DIVERGE that was one known VM gap wearing 12, 9 and 36
  # different hats. Raw output said AGREE=0 DIVERGE=9; the truth was 9 for 9.
  #
  # Keeping the marker itself (so a workload that never reached the end is
  # still distinguishable) and dropping only the hook-sourced field is the
  # narrowest fix: it stops the harness manufacturing divergence without
  # blinding it to a real early exit. The shutdown-hook gap is tracked
  # separately and is NOT excused by this.
  grep -aE '^(CORPUS-START|CORPUS-END|CORPUS-THROW|CORPUS-NOMAIN|SBRUNNER_RESULT) ' "$1" 2>/dev/null \
    | sed 's/\x1b\[[0-9;]*m//g' \
    | sed 's/ completed=[A-Za-z]*//'
}

# A HotSpot arm that started nothing and asserted nothing is a DISAGREEING
# PRECONDITION, not a passing oracle: the workload declined to run on the
# oracle too, so there is no ground truth to compare against. This is the
# same defect regression-suite/harness-guard.sh:166 `harness_guard_oracle`
# guards for in the small-vector suite (its G4). Scoring it green is the
# single most expensive mistake available here, because it converts "we
# learned nothing" into "we verified it".
oracle_vacuous() {
  local logf="$1" line
  line="$(grep -a '^SBRUNNER_RESULT ' "$logf" 2>/dev/null | tail -1)"
  if [ -n "$line" ]; then
    case "$line" in
      *"tests=0"*) echo "SBRUNNER_RESULT reports tests=0 -- the oracle discovered and started nothing"; return 0 ;;
    esac
    # Every discovered test aborted: an `assumeTrue`/`Assumptions.abort` that
    # disagreed with the host. See the memory record on HotSpot oracles that
    # report ok=0 aborted=<all>.
    local t a
    t="$(printf '%s' "$line" | sed -n 's/.*tests=\([0-9]*\).*/\1/p')"
    a="$(printf '%s' "$line" | sed -n 's/.*aborted=\([0-9]*\).*/\1/p')"
    if [ -n "$t" ] && [ -n "$a" ] && [ "$t" -gt 0 ] && [ "$a" -eq "$t" ]; then
      echo "SBRUNNER_RESULT reports every one of $t discovered tests ABORTED -- a disagreeing precondition on the oracle, not a pass"
      return 0
    fi
  fi
  return 1
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
  if [ "$LIMIT" -gt 0 ]; then corpus_discover "$CORPUS_ROOT" | head -n "$LIMIT"
  else corpus_discover "$CORPUS_ROOT"; fi
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
    classes="$(grep -av '^\s*#' "$CLASSES_FROM" | grep -a .)"
  elif [ -n "$WORKCLASS" ]; then
    classes="$WORKCLASS"
  elif [ -n "$CORPUS_DEFAULT_CLASS" ]; then
    classes="$CORPUS_DEFAULT_CLASS"
  else
    die "corpus '$name' declares no default workload; pass --class or --classes-from"
  fi

  # Compose the corpus classpath FIRST: a JUnit runner source needs it to
  # compile against, and the emptiness check below must not be satisfied by
  # the wrapper's own build directory.
  cp_reset
  corpus_classpath "$CORPUS_ROOT" || die "corpus '$name' failed to compose a classpath from $CORPUS_ROOT"
  [ -n "$CP" ] || die "composed classpath is EMPTY for '$name' at $CORPUS_ROOT"
  ensure_wrapper "$CP"
  cp_add "$BUILD"                      # CorpusMain, and the JUnit runner if any

  mkdir -p "$OUT"
  local stamp; stamp="$(date +%Y%m%d-%H%M%S)"
  local rundir="$OUT/$name-$MODE-$stamp"
  mkdir -p "$rundir"
  local tsv="$rundir/results.tsv"
  local argf="$rundir/cp.args"
  write_argfile "$argf" "$CP"

  local wd; wd="$(corpus_workdir "$CORPUS_ROOT")"

  {
    echo "# corpus=$name root=$CORPUS_ROOT mode=$MODE"
    echo "# cv=$CV"
    echo "# jdk=$JDK"
    echo "# timeout=${TIMEOUT}s  cp_entries=$(cp_count)  cp_chars=${#CP}"
    echo "# ms columns are NON-METRIC: this host's wall clock does not support throughput claims."
    printf 'class\tverdict\tcv_state\tcv_rc\tcv_ms\ths_state\ths_rc\ths_ms\tnote\n'
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

  local agree=0 diverge=0 broken=0 unadj=0
  local c
  for c in $classes; do
    local safe; safe="$(printf '%s' "$c" | sed 's/[^A-Za-z0-9_.-]/_/g')"
    local cvlog="$rundir/$safe.cv.log" hslog="$rundir/$safe.hs.log"
    local t0 t1 cvrc hsrc cvms hsms cvstate hsstate verdict note

    # ---- CratonVM arm ----
    # The VM's own watchdog is disabled so that `timeout` is the single
    # authority on the wall; two independent killers make a TIMEOUT row
    # impossible to attribute. regression-suite/run.sh:443 does the same.
    # Run in the corpus's OWN working directory. `$wd` was computed and never
    # used, so both arms ran in the invoking cwd — measured consequences, both
    # of which read as VM findings and were not:
    #   * H2 wrote its databases into the git worktree as untracked `data/`,
    #     and a carried-over corrupt store made `TestBackup` DIVERGE where all
    #     three arms are green when run alone.
    #   * Running Tomcat's `TestSsl` from the right root took the HOTSPOT
    #     ORACLE from failed=7 to failed=1 — six real oracle passes were being
    #     scored as failures, i.e. the harness was manufacturing divergence on
    #     the reference side.
    # A subshell keeps the cd local; every other path here is absolute.
    t0=$(date +%s%3N)
    ( cd "$wd" && CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 timeout "$TIMEOUT" \
      "$CV" --java-home "$JDK" $mflags $CV_ARGS "@$argf" CorpusMain $invoke_prefix "$c" ) \
      > "$cvlog" 2>&1
    cvrc=$?
    t1=$(date +%s%3N); cvms=$((t1-t0))
    cvstate="$(classify_arm "$cvrc" "$cvlog")"

    # ---- HotSpot oracle arm ----
    # The oracle NEVER receives $CV_ARGS or the mode flag: those are CratonVM
    # spellings and the oracle has to stay the plain reference run. And the
    # oracle is HotSpot, never the other CratonVM mode -- a
    # CratonVM-vs-CratonVM comparison proves nothing about correctness,
    # only about self-consistency.
    if [ "$NO_ORACLE" -eq 1 ]; then
      hsstate=SKIPPED; hsrc=-1; hsms=0
      : > "$hslog"
    else
      # Same working directory as the CratonVM arm — see the note there. An
      # oracle run from the wrong cwd manufactures divergence on the REFERENCE
      # side, which is the worst possible place for it: a red oracle row reads
      # as a VM defect and is scored as one.
      t0=$(date +%s%3N)
      ( cd "$wd" && timeout "$TIMEOUT" "$HS" "@$argf" CorpusMain $invoke_prefix "$c" ) \
        > "$hslog" 2>&1
      hsrc=$?
      t1=$(date +%s%3N); hsms=$((t1-t0))
      hsstate="$(classify_arm "$hsrc" "$hslog")"
    fi

    # ---- adjudication ----
    note=""
    if [ "$hsstate" = SKIPPED ]; then
      verdict=UNADJUDICATED; note="--no-oracle: no ground truth was obtained"
      unadj=$((unadj+1))
    elif [ "$hsstate" != RAN ]; then
      # G4-equivalent. The 'expected' side is an artefact of the oracle's own
      # failure, so there is nothing to compare against and this must not be
      # scored either way.
      verdict=ORACLE-UNUSABLE
      note="HotSpot arm state=$hsstate rc=$hsrc -- no ground truth; fix the workload/fixture before reading the CratonVM column"
      unadj=$((unadj+1))
    elif oracle_vacuous "$hslog" > /dev/null; then
      verdict=ORACLE-VACUOUS
      note="$(oracle_vacuous "$hslog")"
      unadj=$((unadj+1))
    elif [ "$cvstate" != RAN ]; then
      verdict="CV-$cvstate"
      case "$cvstate" in
        TIMEOUT) note="killed at ${TIMEOUT}s. On this VM a timeout is very often a SIGSEGV that printed no result line -- treat as a hard failure to be diagnosed, NOT as slowness." ;;
        CRASH)   note="$(grep -aiE 'SIGSEGV|rust panic|fatal runtime error|access violation|stack overflow' "$cvlog" | tail -1 | head -c 160)" ;;
        NOSTART) note="$(grep -a '^CORPUS-NOMAIN ' "$cvlog" | tail -1 | head -c 160)"
                 [ -n "$note" ] || note="no CORPUS-START marker: the workload class never began executing (classpath or launcher problem, not a VM answer)" ;;
      esac
      broken=$((broken+1))
    elif [ "$(arm_key "$cvlog")" = "$(arm_key "$hslog")" ]; then
      verdict=AGREE; agree=$((agree+1))
    else
      verdict=DIVERGE; diverge=$((diverge+1))
      note="markers differ from HotSpot"
      {
        echo "--- HotSpot ---"; arm_key "$hslog"
        echo "--- CratonVM ---"; arm_key "$cvlog"
      } > "$rundir/$safe.diff"
    fi

    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
      "$c" "$verdict" "$cvstate" "$cvrc" "$cvms" "$hsstate" "$hsrc" "$hsms" "$note" >> "$tsv"
    printf '  %-52s %-16s cv=%-8s hs=%-8s %s\n' "$c" "$verdict" "$cvstate" "$hsstate" "$note"
  done

  echo
  echo "corpus=$name mode=$MODE  AGREE=$agree DIVERGE=$diverge CV-BROKEN=$broken UNADJUDICATED=$unadj"
  echo "results: $tsv"
  if [ "$unadj" -gt 0 ] && [ "$agree" -eq 0 ] && [ "$diverge" -eq 0 ] && [ "$broken" -eq 0 ]; then
    echo "NOTHING WAS ADJUDICATED. This run is not evidence about the VM." >&2
    return 2
  fi
  [ "$diverge" -eq 0 ] && [ "$broken" -eq 0 ] && return 0
  return 1
}

# --- entry point ------------------------------------------------------------

sub="${1:-}"; shift || true
case "$sub" in
  list)     cmd_list "$@" ;;
  info)     cmd_info "$@" ;;
  discover) cmd_discover "$@" ;;
  run)      cmd_run "$@" ;;
  ""|-h|--help|help)
    sed -n '2,50p' "$HERE/run-corpus.sh" | sed 's/^# \{0,1\}//'
    exit 3 ;;
  *) usage_die "unknown subcommand '$sub'" ;;
esac
