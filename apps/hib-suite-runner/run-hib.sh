#!/usr/bin/env bash
# =============================================================================
# run-hib.sh — CratonVM Hibernate ORM test-suite runner
#
# Runs Hibernate ORM JUnit5 test classes on CratonVM, fork-per-class (so a
# crash/hang in one class never stops the rest), with per-class timing and full
# logs persisted to disk.
#
# Classes are split into two categories (see passed.txt / others.txt):
#   passed  — classes that pass on CratonVM
#   others  — everything else (fail / hang / crash / loaderror)
#
# You choose: category, how many classes, the starting index, JIT on/off, and
# real-JDK vs synthetic-JDK mode. Or run all 4 (JIT × JDK) modes concurrently,
# one per background "thread".
#
# A few classes need more than the flat per-class wall cap, or need specific VM
# flags. Those accommodations live in the TRACKED table `class-overrides.tsv`
# next to this script (see that file's header for why it is tracked and how to
# check it is actually loaded).
#
# A separate handful of classes report class-level ABORTED for reasons that
# have nothing to do with CratonVM (JUnit `Assumptions.abort(...)` self-skips
# baked into the test, byte-identical on HotSpot). `categorize` treats those
# as pass-equivalent via the TRACKED table `known-benign-aborts.tsv`, next to
# this script — see that file's header for the exact-count-match rule that
# keeps a real regression from being silently swallowed.
#
# Any extra CratonVM tuning goes through the environment: every CRATONVM_* env
# var is inherited by the VM child processes automatically.
# =============================================================================
set -uo pipefail
export MSYS2_ARG_CONV_EXCL='*'
export MSYS_NO_PATHCONV=1

# --- locations ----------------------------------------------------------------
# $HERE resolves to wherever THIS script actually lives, not a hardcoded
# checkout path -- so a worktree with its own generated fixture data
# ($COMMON, the test lists, the compiled runner -- none of them tracked in
# git) is fully self-contained, while a copy of the script that still sits
# next to the original fixture data keeps resolving there exactly as before.
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" 2>/dev/null && { pwd -W 2>/dev/null || pwd; })"
COMMON="$HERE/common.args"          # -cp + sysprops + junit timeout
RUNNER_CLASS="CratonRunner"         # compiled in $HERE, already on the classpath
# The override table below is tracked, so it exists next to whichever copy of
# this script is being executed -- same $SELF_DIR-vs-$HERE fallback shape.
SELF_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" 2>/dev/null && pwd)"
OVERRIDES="${HIB_CLASS_OVERRIDES:-}"
if [ -z "$OVERRIDES" ]; then
  if [ -f "$SELF_DIR/class-overrides.tsv" ]; then OVERRIDES="$SELF_DIR/class-overrides.tsv"
  else OVERRIDES="$HERE/class-overrides.tsv"; fi
fi
# Same story for the known-benign-ABORTED table consulted by `categorize`.
BENIGN_ABORTS="${HIB_KNOWN_BENIGN_ABORTS:-}"
if [ -z "$BENIGN_ABORTS" ]; then
  if [ -f "$SELF_DIR/known-benign-aborts.tsv" ]; then BENIGN_ABORTS="$SELF_DIR/known-benign-aborts.tsv"
  else BENIGN_ABORTS="$HERE/known-benign-aborts.tsv"; fi
fi
# ...and for the required-sysprops table, which exists because $COMMON itself is
# generated and untracked and so can silently lose a load-bearing -D.
REQ_SYSPROPS="${HIB_REQUIRED_SYSPROPS:-}"
if [ -z "$REQ_SYSPROPS" ]; then
  if [ -f "$SELF_DIR/required-sysprops.tsv" ]; then REQ_SYSPROPS="$SELF_DIR/required-sysprops.tsv"
  else REQ_SYSPROPS="$HERE/required-sysprops.tsv"; fi
fi

# --- JDK autodetect ----------------------------------------------------------
# The real JDK moves around on this box (it has been `Program Files/Java/jdk-25`
# and `Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot`). A hardcoded dead
# default fails silently-ish: --java-home just breaks inside every forked VM and
# the whole run reports CRASH per class. Probe for a real one instead, and
# hard-fail with a usable message if none is found.
detect_jdk() {
  local c
  for c in "C:/Program Files/Eclipse Adoptium"/jdk-25* \
           "C:/Program Files/Java"/jdk-25* \
           "C:/Program Files/Eclipse Adoptium"/jdk-2* \
           "C:/Program Files/Java"/jdk-2*; do
    [ -x "$c/bin/java.exe" ] && { printf "%s" "$c"; return 0; }; [ -x "$c/bin/java" ] && { printf "%s" "$c"; return 0; }
  done
  return 1
}

# --- defaults (override via flags or env) ------------------------------------
CV_BIN="${CV_BIN:-C:/craton/CratonVM-hibtest/target/release/cratonvm.exe}"
JDK="${JDK:-$(detect_jdk)}"
CV_XMX="${CV_XMX:-1500m}"
SHARDS="${SHARDS:-6}"               # parallel forks per mode
TIMEOUT="${TIMEOUT:-300}"          # per-class wall cap (s) -> HANG
OUTROOT="${OUTROOT:-$HERE/runs}"
USE_OVERRIDES=1                     # --no-overrides disables the table (A/B)
# --pg-worker-base <N> (0 = off): assigns each shard s (0-based) its own
# Postgres worker database hibernate_orm_test_<N+s+1> via a direct
# -Dhibernate.connection.url override, bypassing GradleParallelTestingResolver's
# $worker templating entirely. That resolver keys its worker-id sequence file
# by the JVM's PARENT PID (see GradleParallelTestingResolver.getWorkerID) --
# under this harness's fork-per-class model every invocation gets a fresh
# parent (a new `timeout` process), so the sequence file is always empty and
# every fork resolves to worker=1 regardless of maxParallelForks. Concurrent
# shards -- let alone concurrent GC arms run via separate --pg-worker-base
# offsets -- would otherwise all hit hibernate_orm_test_1 at once. A literal
# resolved URL with no "$worker" substring passes through
# GradleParallelTestingConnectionCreatorFactoryImpl.resolveUrl() unchanged
# (nothing to replace), and system properties override hibernate.properties
# (Environment's own documented precedence), so this cleanly wins.
#
# Empty string = disabled. NOT 0 -- 0 is a legitimate base (the first GC arm's
# shards land on workers 1..N). An earlier version used 0 as the disabled
# sentinel, which silently disabled isolation for whichever arm was launched
# with --pg-worker-base 0: all its shards fell through to the resolver's
# always-worker=1 default and collided on hibernate_orm_test_1 concurrently.
PG_WORKER_BASE="${PG_WORKER_BASE:-}"
# --mysql-worker-base <N>: same mechanism as --pg-worker-base above, for the
# MySQL-in-Docker setup (hibernate.properties now points at
# jdbc:mysql://localhost/hibernate_orm_test_$worker -- see
# hibernate-orm/hibernate-core/target/resources/test/hibernate.properties).
# Empty string = disabled, same non-zero-sentinel reasoning as PG_WORKER_BASE.
MYSQL_WORKER_BASE="${MYSQL_WORKER_BASE:-}"

CATEGORY="passed"
JITMODE="on"
JDKMODE="real"
COUNT="0"                           # 0 = all from START to end
START="0"
ALLMODES="0"

usage() {
  cat <<'USAGE'
run-hib.sh — run the Hibernate ORM suite on CratonVM

USAGE:
  run-hib.sh [options]

OPTIONS:
  --category <passed|others>   which list to run (default: passed)
  --count <N>                  number of classes to run (default: 0 = all)
  --start <IDX>                0-based start index into the category list (default: 0)
  --jit <on|off>               JIT on, or off via --nojit (default: on)
  --jdk <real|synthetic>       real-JDK (--java-home) or synthetic (--synthetic-jdk) (default: real)
  --all-modes                  run all 4 modes (jit{on,off} x jdk{real,synthetic}) concurrently,
                               each in its own background thread (ignores --jit/--jdk)
  --timeout <SEC>              per-class hang timeout (default: 300); per-class
                               overrides raise this as a floor, never lower it
  --no-overrides               ignore class-overrides.tsv entirely (A/B checks)
  --no-sysprops                do not inject required-sysprops.tsv entries (A/B checks)
  --shards <N>                 parallel forks per mode (default: 6)
  --pg-worker-base <N>          per-shard Postgres worker DB isolation (see comment at definition)
  --mysql-worker-base <N>       per-shard MySQL worker DB isolation, same mechanism as above
  --bin <path>                 cratonvm.exe (default: $CV_BIN env or hibtest release)
  --out <dir>                  output root (default: ./runs)
  -h | --help

SUB-COMMANDS:
  run-hib.sh categorize        rebuild passed.txt / others.txt from a full run
  run-hib.sh overrides         print the loaded per-class override table and exit
  run-hib.sh benign-aborts     print the loaded known-benign-aborts table and exit
  run-hib.sh sysprops          print required-sysprops.tsv and which entries this
                               host's common.args is missing, then exit

CATEGORIES (regenerate authoritatively with:  run-hib.sh categorize):
  passed.txt  / others.txt    in this folder

PER-CLASS OVERRIDES:
  class-overrides.tsv (tracked in git, next to this script) raises the wall cap
  and/or adds VM flags for individual classes that the flat default misjudges.
  Every run prints `overrides=N (loaded)` in its mode header; `overrides=0
  (MISSING ...)` means the table is gone and known-slow classes will be
  misreported as HANG. Check it with `run-hib.sh overrides`.

KNOWN-BENIGN ABORTS:
  known-benign-aborts.tsv (tracked in git, next to this script) lists classes
  whose class-level ABORTED status is a confirmed HotSpot-parity JUnit
  Assumptions self-skip, not a CratonVM bug. `categorize` treats a class as
  pass-equivalent (routes it to passed.txt instead of others.txt) only when
  its found/ok/aborted counts match the table EXACTLY, so a real regression
  on one of these classes still surfaces. Check it with
  `run-hib.sh benign-aborts`.

REQUIRED SYSPROPS:
  common.args is generated and untracked, so a -D the suite cannot run without
  has no authoritative home and goes missing on a fresh host --
  required-sysprops.tsv (tracked, next to this script) is that home. Any entry
  the argfile does not already carry is injected ahead of it, and every run
  prints `sysprops=N (M injected)`. Check it with `run-hib.sh sysprops`.

ENV PASS-THROUGH:
  Any CRATONVM_* variable in your environment is inherited by the VM, e.g.
    CRATONVM_TIER_C2_THRESHOLD=200 run-hib.sh --count 50
  Other knobs: CV_BIN, JDK, CV_XMX, SHARDS, TIMEOUT, OUTROOT

EXAMPLES:
  # first 200 passing classes, default (JIT on, real JDK)
  run-hib.sh --category passed --count 200
  # known-failing classes 0..50, JIT off
  run-hib.sh --category others --count 50 --jit off
  # classes 500..1000 of the passing set, synthetic JDK
  run-hib.sh --category passed --start 500 --count 500 --jdk synthetic
  # all 4 modes at once over the first 100 passing classes
  run-hib.sh --category passed --count 100 --all-modes
  # rebuild passed.txt / others.txt from a fresh full run
  run-hib.sh categorize
USAGE
}

# --- per-class override table ------------------------------------------------
# Tab-separated: <class> <timeout-seconds|-> <extra VM flags|->.  The timeout is
# a FLOOR (max with the run's own --timeout), never a cap; the flags are
# appended ahead of the @common.args argfile and de-duplicated.  The table lives
# in a tracked file precisely so it cannot silently disappear the way the
# 2026-07-22 accommodation for DefaultCatalogAndSchemaTest did.
declare -A CLASS_TIMEOUT_OVERRIDE=()
declare -A CLASS_FLAGS_OVERRIDE=()
OVERRIDES_STATE="disabled"

load_overrides() {
  CLASS_TIMEOUT_OVERRIDE=(); CLASS_FLAGS_OVERRIDE=()
  if [ "$USE_OVERRIDES" != 1 ]; then OVERRIDES_STATE="disabled (--no-overrides)"; return 0; fi
  if [ ! -f "$OVERRIDES" ]; then
    OVERRIDES_STATE="MISSING $OVERRIDES"
    echo "WARNING: per-class override table not found: $OVERRIDES" >&2
    echo "WARNING: known-slow classes will be killed at the flat ${TIMEOUT}s cap and reported HANG." >&2
    return 0
  fi
  local cls to fl _rest
  # `${x%$'\r'}` on every field: this file is edited on Windows and can arrive
  # with CRLF endings, which would otherwise poison the class-name key.
  while IFS=$'\t' read -r cls to fl _rest || [ -n "${cls:-}" ]; do
    cls="${cls%$'\r'}"; to="${to:-}"; to="${to%$'\r'}"; fl="${fl:-}"; fl="${fl%$'\r'}"
    case "$cls" in ''|\#*) continue;; esac
    if [ -n "$to" ] && [ "$to" != "-" ]; then
      # Reject a non-numeric timeout loudly. Silently ignoring it would restore
      # exactly the failure this table exists to prevent.
      case "$to" in
        *[!0-9]*|'') echo "WARNING: $OVERRIDES: non-numeric timeout '$to' for $cls — ignored" >&2;;
        *) CLASS_TIMEOUT_OVERRIDE["$cls"]="$to";;
      esac
    fi
    [ -n "$fl" ] && [ "$fl" != "-" ] && CLASS_FLAGS_OVERRIDE["$cls"]="$fl"
  done < "$OVERRIDES"
  local n=$(( ${#CLASS_TIMEOUT_OVERRIDE[@]} > ${#CLASS_FLAGS_OVERRIDE[@]} ? ${#CLASS_TIMEOUT_OVERRIDE[@]} : ${#CLASS_FLAGS_OVERRIDE[@]} ))
  OVERRIDES_STATE="$n (loaded)"
}

print_overrides() {
  echo "override table: $OVERRIDES"
  echo "state: $OVERRIDES_STATE"
  local k
  for k in "${!CLASS_TIMEOUT_OVERRIDE[@]}"; do
    printf '  %s  timeout=%ss flags=%s\n' "$k" "${CLASS_TIMEOUT_OVERRIDE[$k]}" "${CLASS_FLAGS_OVERRIDE[$k]:--}"
  done
  for k in "${!CLASS_FLAGS_OVERRIDE[@]}"; do
    [ -n "${CLASS_TIMEOUT_OVERRIDE[$k]:-}" ] && continue
    printf '  %s  timeout=-  flags=%s\n' "$k" "${CLASS_FLAGS_OVERRIDE[$k]}"
  done
}

# --- known-benign-aborts table: confirmed HotSpot-parity ABORTED classes -----
# `categorize` (below) treats a class as pass-equivalent only when its
# found/ok/aborted counts match this table EXACTLY, so a future regression
# that changes the abort profile (a new failure appears, or the count of
# self-skips shifts) still lands in others.txt like any other residual
# instead of being silently swallowed. See the table's own header comment
# for format and rationale.
declare -A BENIGN_FOUND=() BENIGN_OK=() BENIGN_ABORTED=()
BENIGN_STATE="not loaded"

load_benign_aborts() {
  BENIGN_FOUND=(); BENIGN_OK=(); BENIGN_ABORTED=()
  if [ ! -f "$BENIGN_ABORTS" ]; then
    BENIGN_STATE="MISSING $BENIGN_ABORTS"
    echo "WARNING: known-benign-aborts table not found: $BENIGN_ABORTS" >&2
    echo "WARNING: confirmed-benign ABORTED classes will be re-flagged into others.txt by the next categorize run." >&2
    return 0
  fi
  local cls ef eo ea _rest
  while IFS=$'\t' read -r cls ef eo ea _rest || [ -n "${cls:-}" ]; do
    cls="${cls%$'\r'}"; ef="${ef%$'\r'}"; eo="${eo%$'\r'}"; ea="${ea%$'\r'}"
    case "$cls" in ''|\#*) continue;; esac
    BENIGN_FOUND["$cls"]="$ef"; BENIGN_OK["$cls"]="$eo"; BENIGN_ABORTED["$cls"]="$ea"
  done < "$BENIGN_ABORTS"
  BENIGN_STATE="${#BENIGN_FOUND[@]} (loaded)"
}

print_benign_aborts() {
  load_benign_aborts
  echo "known-benign-aborts table: $BENIGN_ABORTS"
  echo "state: $BENIGN_STATE"
  local k
  for k in "${!BENIGN_FOUND[@]}"; do
    printf '  %s  found=%s ok=%s aborted=%s\n' "$k" "${BENIGN_FOUND[$k]}" "${BENIGN_OK[$k]}" "${BENIGN_ABORTED[$k]}"
  done
}

# --- required-sysprops table: -D lines $COMMON is not allowed to be missing ---
# $COMMON is GENERATED and UNTRACKED (hand-built, or dumped by
# cratonvm-dump.gradle -- which only ever emits a classpath, never sysprops), so
# a sysprop the suite cannot run without has no authoritative home and is simply
# absent on a fresh host. On 2026-08-11 that cost a whole 4579-class Azure Linux
# run: FAIL=305 vs FAIL=4 on Windows, 246 of them one
# `IllegalStateException: BytecodeEnhancedTestEngine is disabled` thrown by
# Hibernate's own JUnit extension -- identical under real HotSpot, i.e. not a VM
# bug at all. Inject every entry the argfile does not already carry, ahead of
# the argfile, and SAY SO in the mode header rather than papering over it.
declare -a INJECTED_SYSPROPS=()
SYSPROPS_STATE="not loaded"
USE_SYSPROPS="${USE_SYSPROPS:-1}"

load_required_sysprops() {
  INJECTED_SYSPROPS=()
  if [ "$USE_SYSPROPS" != 1 ]; then SYSPROPS_STATE="disabled (--no-sysprops)"; return 0; fi
  if [ ! -f "$REQ_SYSPROPS" ]; then
    SYSPROPS_STATE="MISSING $REQ_SYSPROPS"
    echo "WARNING: required-sysprops table not found: $REQ_SYSPROPS" >&2
    echo "WARNING: if common.args is also missing one, whole test families fail for a config reason and look like VM bugs." >&2
    return 0
  fi
  local key val _why total=0
  while IFS=$'\t' read -r key val _why || [ -n "${key:-}" ]; do
    key="${key%$'\r'}"; val="${val:-}"; val="${val%$'\r'}"
    case "$key" in ''|\#*) continue;; esac
    total=$((total+1))
    # Already set in the argfile at ANY value? Leave it alone -- a deliberate
    # local override must still win over this table.
    if [ -f "$COMMON" ] && grep -q -- "-D$key=" "$COMMON" 2>/dev/null; then continue; fi
    INJECTED_SYSPROPS+=("-D$key=$val")
  done < "$REQ_SYSPROPS"
  SYSPROPS_STATE="$total (${#INJECTED_SYSPROPS[@]} injected)"
}

print_required_sysprops() {
  load_required_sysprops
  echo "required-sysprops table: $REQ_SYSPROPS"
  echo "argfile: $COMMON"
  echo "state: $SYSPROPS_STATE"
  local key val why
  while IFS=$'\t' read -r key val why || [ -n "${key:-}" ]; do
    key="${key%$'\r'}"; val="${val:-}"; val="${val%$'\r'}"; why="${why:-}"; why="${why%$'\r'}"
    case "$key" in ''|\#*) continue;; esac
    if [ -f "$COMMON" ] && grep -q -- "-D$key=" "$COMMON" 2>/dev/null; then
      printf '  [in argfile] -D%s=%s\n' "$key" "$(grep -o -- "-D$key=[^ ]*" "$COMMON" | head -1 | cut -d= -f2-)"
    else
      printf '  [INJECTED  ] -D%s=%s\n      why: %s\n' "$key" "$val" "$why"
    fi
  done < "$REQ_SYSPROPS"
}

# --- categorize sub-command: (re)build passed.txt / others.txt ----------------
if [ "${1:-}" = "categorize" ]; then
  echo "[categorize] running the full testlist once (JIT on, real JDK) to split passed/others ..."
  CATEGORIZE=1
else
  CATEGORIZE=0
fi

# --- arg parse ---------------------------------------------------------------
SHOW_OVERRIDES=0
SHOW_BENIGN_ABORTS=0
SHOW_SYSPROPS=0
EXPLICIT_LIST=""
while [ $# -gt 0 ]; do
  case "$1" in
    categorize) shift;;
    overrides)  SHOW_OVERRIDES=1; shift;;
    benign-aborts) SHOW_BENIGN_ABORTS=1; shift;;
    sysprops)   SHOW_SYSPROPS=1; shift;;
    --no-overrides) USE_OVERRIDES=0; shift;;
    --no-sysprops)  USE_SYSPROPS=0; shift;;
    --category) CATEGORY="$2"; shift 2;;
    --count)    COUNT="$2"; shift 2;;
    --start)    START="$2"; shift 2;;
    --jit)      JITMODE="$2"; shift 2;;
    --jdk)      JDKMODE="$2"; shift 2;;
    --all-modes) ALLMODES=1; shift;;
    --timeout)  TIMEOUT="$2"; shift 2;;
    --shards)   SHARDS="$2"; shift 2;;
    --pg-worker-base) PG_WORKER_BASE="$2"; shift 2;;
    --mysql-worker-base) MYSQL_WORKER_BASE="$2"; shift 2;;
    --bin)      CV_BIN="$2"; shift 2;;
    --out)      OUTROOT="$2"; shift 2;;
    --list)     EXPLICIT_LIST="$2"; shift 2;;
    -h|--help)  usage; exit 0;;
    *) echo "unknown option: $1" >&2; usage; exit 2;;
  esac
done

load_overrides
if [ "$SHOW_OVERRIDES" = 1 ]; then print_overrides; exit 0; fi
if [ "$SHOW_BENIGN_ABORTS" = 1 ]; then print_benign_aborts; exit 0; fi
if [ "$SHOW_SYSPROPS" = 1 ]; then print_required_sysprops; exit 0; fi

# The forked VMs inherit this script's working directory, and Hibernate's own
# test infrastructure resolves its JDBC URL through
# `GradleParallelTestingResolver.getWorkerID`, which reads a worker-id file
# relative to the CWD. Launched from anywhere but $HERE that read throws
# FileNotFoundException -> "An error occurred when computing worker ID" ->
# ExceptionInInitializerError in JdbcConnectionContext's <clinit>, and EVERY
# class in the run is recorded as CRASH within a second. Pin the CWD so the
# script is safe to invoke by absolute path from anywhere — resolving any
# caller-relative --bin/--out against the ORIGINAL cwd first.
ORIG_PWD="$PWD"
abspath() { case "$1" in /*|[A-Za-z]:[/\\]*) printf '%s' "$1";; *) printf '%s/%s' "$ORIG_PWD" "$1";; esac; }
CV_BIN="$(abspath "$CV_BIN")"
OUTROOT="$(abspath "$OUTROOT")"
[ -n "$EXPLICIT_LIST" ] && EXPLICIT_LIST="$(abspath "$EXPLICIT_LIST")"
cd "$HERE" || { echo "ERROR: cannot cd to fixture dir: $HERE" >&2; exit 1; }

[ -f "$CV_BIN" ] || { echo "ERROR: cratonvm binary not found: $CV_BIN (set --bin or CV_BIN)" >&2; exit 1; }
[ -f "$COMMON" ] || { echo "ERROR: common.args not found: $COMMON" >&2; exit 1; }
load_required_sysprops
if [ ${#INJECTED_SYSPROPS[@]} -gt 0 ]; then
  echo "[sysprops] $COMMON is missing ${#INJECTED_SYSPROPS[@]} required -D; injecting: ${INJECTED_SYSPROPS[*]}" >&2
fi
[ -x "$JDK/bin/java.exe" ] || [ -x "$JDK/bin/java" ] || { echo "ERROR: real JDK not found: '${JDK:-<none detected>}' (set --jdk-home via JDK=... env)" >&2; exit 1; }
mkdir -p "$OUTROOT"
TS="$(date +%Y%m%d-%H%M%S)"

# --- fork-per-class runner for one shard -------------------------------------
# NOTE: this CratonRunner build takes class names directly as argv (one JVM
# call per class here, for crash/hang isolation) and prints a single line
# `@@RESULT <className> found=.. started=.. ok=.. failed=.. aborted=.. skipped=.. ms=..`
# plus `@@BATCHEND failed_classes=N` — no @@BEGIN/@@FAIL/index markers, no
# listfile/start-index argument support.
# args: $1=listfile $2=shard-outdir ; reads global VMFLAGS_BASE array plus the
# CLASS_TIMEOUT_OVERRIDE / CLASS_FLAGS_OVERRIDE tables
run_shard() {
  local LIST="$1" OUT="$2" SHARD_IDX="${3:-0}"
  mkdir -p "$OUT"
  local RAW="$OUT/raw.log" TSV="$OUT/results.tsv"
  : > "$RAW"
  printf 'idx\tclass\tstatus\tfound\tok\tfailed\taborted\tskipped\tms\tsig\n' > "$TSV"
  local idx=0 cls tmp rc
  # see PG_WORKER_BASE's definition above for why this bypasses the resolver
  local -a PG_URL_FLAG=()
  if [ -n "$PG_WORKER_BASE" ]; then
    local worker_n=$((PG_WORKER_BASE + SHARD_IDX + 1))
    PG_URL_FLAG=(-Dhibernate.connection.url="jdbc:postgresql://localhost/hibernate_orm_test_${worker_n}?preparedStatementCacheQueries=0&escapeSyntaxCallMode=callIfNoReturn")
    echo "[pg-worker] shard $SHARD_IDX -> hibernate_orm_test_${worker_n}" >> "$RAW"
  fi
  local -a MYSQL_URL_FLAG=()
  if [ -n "$MYSQL_WORKER_BASE" ]; then
    local myworker_n=$((MYSQL_WORKER_BASE + SHARD_IDX + 1))
    MYSQL_URL_FLAG=(-Dhibernate.connection.url="jdbc:mysql://localhost/hibernate_orm_test_${myworker_n}?allowPublicKeyRetrieval=true&useSSL=false")
    echo "[mysql-worker] shard $SHARD_IDX -> hibernate_orm_test_${myworker_n}" >> "$RAW"
  fi
  while IFS= read -r cls; do
    [ -z "$cls" ] && continue
    # --- per-class accommodation (class-overrides.tsv) ------------------------
    # timeout is a floor: a run that already asks for longer keeps its own value.
    local cls_to cls_fl eff_to f
    local -a eff_flags=("${VMFLAGS_BASE[@]}" "${PG_URL_FLAG[@]}" "${MYSQL_URL_FLAG[@]}")
    cls_to="${CLASS_TIMEOUT_OVERRIDE[$cls]:-}"
    cls_fl="${CLASS_FLAGS_OVERRIDE[$cls]:-}"
    eff_to="$TIMEOUT"
    if [ -n "$cls_to" ] && [ "$cls_to" -gt "$TIMEOUT" ] 2>/dev/null; then eff_to="$cls_to"; fi
    if [ -n "$cls_fl" ]; then
      for f in $cls_fl; do
        case " ${eff_flags[*]} " in *" $f "*) ;; *) eff_flags+=("$f");; esac
      done
    fi
    if [ -n "$cls_to" ] || [ -n "$cls_fl" ]; then
      echo "[override] $cls timeout=${eff_to}s (default ${TIMEOUT}s) extra_flags=${cls_fl:--}" >> "$RAW"
      echo "[override] $cls timeout=${eff_to}s extra_flags=${cls_fl:--}" >> "$OUT/../overrides.log"
    fi
    eff_flags+=(@"$COMMON")

    tmp=$(mktemp)
    CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 timeout "$eff_to" "$CV_BIN" "${eff_flags[@]}" \
        -Dcraton.batch=1 "$RUNNER_CLASS" "$cls" >"$tmp" 2>>"$RAW"; rc=$?
    cat "$tmp" >> "$RAW"
    local rline found ok failed aborted skipped ms status sig
    # NOT anchored at line start, and matched against THIS class's name.
    #
    # `^@@RESULT ` was, and it silently converted a clean PASS into a CRASH.
    # `CratonRunner` writes its result line to the same `System.out` the test
    # bodies write to, and a test that ends with a newline-less write leaves the
    # cursor mid-line: `bootstrap.scanning.JarVisitorTest` finishes with
    # `System.out.printf("InputStream byte[] extraction algorithms; ...")` and
    # no `%n`, so the run emits
    #     InputStream byte[] extraction algorithms; old = `59`, new = `17`@@RESULT org.hibernate...
    # The anchored grep found nothing, the row was recorded
    # `CRASH ... process-died rc=0`, and a class that had just reported
    # `found=9 ok=9 failed=0` was carried into a known-issues doc as a
    # VM-vs-HotSpot divergence (2026-08-11 Azure Linux full suite). rc=0 with no
    # result line is a PARSE failure far more often than a VM failure.
    #
    # Including `$cls` keeps `-o` from picking up a `@@RESULT`-looking string a
    # test body printed for some other class.
    rline=$(grep -o "@@RESULT $cls found=.*" "$tmp" | head -1)
    if [ -n "$rline" ]; then
      found=$(printf '%s' "$rline"|grep -o 'found=[0-9]*'|cut -d= -f2); ok=$(printf '%s' "$rline"|grep -o 'ok=[0-9]*'|cut -d= -f2)
      failed=$(printf '%s' "$rline"|grep -o 'failed=[0-9]*'|cut -d= -f2); aborted=$(printf '%s' "$rline"|grep -o 'aborted=[0-9]*'|cut -d= -f2)
      skipped=$(printf '%s' "$rline"|grep -o 'skipped=[0-9]*'|cut -d= -f2); ms=$(printf '%s' "$rline"|grep -o 'ms=[0-9]*'|cut -d= -f2)
      status=PASS
      if [ "${failed:-0}" -gt 0 ]; then status=FAIL
      elif [ "${aborted:-0}" -gt 0 ]; then status=ABORTED
      elif [ "${found:-0}" -eq 0 ]; then status=NOTESTS; fi
      sig=""
      if [ "$status" = FAIL ]; then sig=$(grep -m1 -E "^(MethodSource|[A-Za-z][A-Za-z0-9_.]*(Exception|Error))" "$tmp" | head -c 160); fi
      printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$idx" "$cls" "$status" "${found:-0}" "${ok:-0}" "${failed:-0}" "${aborted:-0}" "${skipped:-0}" "${ms:-0}" "$sig" >> "$TSV"
    else
      local st; if [ "$rc" -eq 124 ]; then st=HANG; else st=CRASH; fi
      # record the cap that actually killed it, so a HANG row can never again be
      # read as "stuck" when it merely outran a too-short per-class wall cap.
      printf '%s\t%s\t%s\t0\t0\t0\t0\t0\t0\t%s rc=%s timeout=%ss\n' "$idx" "$cls" "$st" "process-died" "$rc" "$eff_to" >> "$TSV"
    fi
    rm -f "$tmp"
    idx=$((idx+1))
  done < "$LIST"
}

# --- run one mode (sharded) --------------------------------------------------
# args: $1=label  $2=jit(on|off)  $3=jdk(real|synthetic)  $4=slice-listfile  $5=mode-outdir
run_mode() {
  local label="$1" jit="$2" jdk="$3" SLICE="$4" MODE="$5"
  mkdir -p "$MODE"
  # BASE = everything except the @argfile; run_shard appends any per-class flags
  # and then the argfile, so a class override lands ahead of the -cp/-D block.
  VMFLAGS_BASE=(--java-home "$JDK" --Xmx "$CV_XMX")
  [ "$jit" = off ] && VMFLAGS_BASE+=(--nojit)
  [ "$jdk" = synthetic ] && VMFLAGS_BASE+=(--synthetic-jdk)
  # Sysprops the argfile is missing go in ahead of it, same as a class override.
  [ ${#INJECTED_SYSPROPS[@]} -gt 0 ] && VMFLAGS_BASE+=("${INJECTED_SYSPROPS[@]}")
  local n; n=$(grep -c '' "$SLICE")
  echo "[$label] $n classes | jit=$jit jdk=$jdk shards=$SHARDS timeout=${TIMEOUT}s overrides=$OVERRIDES_STATE sysprops=$SYSPROPS_STATE bin=$CV_BIN"
  local t0; t0=$(date +%s)
  local s pids=()
  for ((s=0; s<SHARDS; s++)); do awk -v n="$SHARDS" -v r="$s" 'NR%n==r' "$SLICE" > "$MODE/shard-$s.txt"; done
  for ((s=0; s<SHARDS; s++)); do ( run_shard "$MODE/shard-$s.txt" "$MODE/shard-$s" "$s" ) & pids+=($!); done
  for p in "${pids[@]}"; do wait "$p"; done
  local t1; t1=$(date +%s); local secs=$((t1-t0))
  # merge
  local MERGED="$MODE/results.tsv"
  head -1 "$MODE/shard-0/results.tsv" > "$MERGED" 2>/dev/null
  for ((s=0; s<SHARDS; s++)); do tail -n +2 "$MODE/shard-$s/results.tsv" 2>/dev/null; done >> "$MERGED"
  local rec; rec=$(( $(grep -c '' "$MERGED") - 1 ))
  {
    echo "mode=$label jit=$jit jdk=$jdk classes=$n recorded=$rec wall_seconds=$secs ($((secs/60))m$((secs%60))s) overrides=$OVERRIDES_STATE sysprops=$SYSPROPS_STATE"
    awk -F'\t' 'NR>1{c[$3]++; tms+=$9} END{printf "status:"; for(k in c) printf " %s=%d",k,c[k]; printf "  sum_class_ms=%d\n",tms}' "$MERGED"
  } | tee "$MODE/SUMMARY.txt"
}

# --- resolve category list ---------------------------------------------------
if [ -n "$EXPLICIT_LIST" ]; then
  SRCLIST="$EXPLICIT_LIST"
elif [ "$CATEGORIZE" = 1 ]; then
  SRCLIST="$HERE/testlist.txt"
else
  case "$CATEGORY" in
    passed) SRCLIST="$HERE/passed.txt";;
    others|failed) SRCLIST="$HERE/others.txt";;
    *) echo "bad --category: $CATEGORY" >&2; exit 2;;
  esac
fi
[ -f "$SRCLIST" ] || { echo "ERROR: list not found: $SRCLIST" >&2; exit 1; }

# --- slice [START, START+COUNT) ----------------------------------------------
SLICE="$OUTROOT/.slice-$TS.txt"
if [ "$COUNT" -gt 0 ]; then
  awk -v s="$START" -v c="$COUNT" 'NR>s && NR<=s+c' "$SRCLIST" > "$SLICE"
else
  awk -v s="$START" 'NR>s' "$SRCLIST" > "$SLICE"
fi
SLN=$(grep -c '' "$SLICE")
echo "=== run-hib $TS :: category=$CATEGORY start=$START count=$COUNT -> $SLN classes ==="

# --- categorize: run full list (jit on/real), then split ---------------------
if [ "$CATEGORIZE" = 1 ]; then
  RUN="$OUTROOT/categorize-$TS"
  run_mode "categorize" on real "$SLICE" "$RUN"
  load_benign_aborts
  : > "$HERE/passed.txt"; : > "$HERE/others.txt"
  RECLASSIFIED=0
  while IFS=$'\t' read -r ridx rcls rstatus rfound rok rfailed rabort rskip rms rsig; do
    [ "$ridx" = "idx" ] && continue     # header row
    [ -z "${ridx:-}" ] && continue
    if [ "$rstatus" = "PASS" ]; then
      echo "$rcls" >> "$HERE/passed.txt"
    elif [ "$rstatus" = "ABORTED" ] && [ -n "${BENIGN_FOUND[$rcls]:-}" ] \
         && [ "$rfound" = "${BENIGN_FOUND[$rcls]}" ] \
         && [ "$rok" = "${BENIGN_OK[$rcls]}" ] \
         && [ "$rabort" = "${BENIGN_ABORTED[$rcls]}" ]; then
      # Confirmed HotSpot-parity self-skip, exact-count match: pass-equivalent.
      echo "$rcls" >> "$HERE/passed.txt"
      RECLASSIFIED=$((RECLASSIFIED+1))
    else
      echo "$rcls" >> "$HERE/others.txt"
    fi
  done < "$RUN/results.tsv"
  sort -u -o "$HERE/passed.txt" "$HERE/passed.txt"
  sort -u -o "$HERE/others.txt" "$HERE/others.txt"
  echo "rebuilt passed.txt=$(grep -c '' "$HERE/passed.txt")  others.txt=$(grep -c '' "$HERE/others.txt")  known-benign-aborts=$BENIGN_STATE reclassified=$RECLASSIFIED"
  rm -f "$SLICE"; exit 0
fi

# --- single mode or all 4 modes concurrently ---------------------------------
RUN="$OUTROOT/run-$TS-$CATEGORY"
mkdir -p "$RUN"
echo "output: $RUN"
GT0=$(date +%s)
if [ "$ALLMODES" = 1 ]; then
  declare -a MPIDS=()
  run_mode "jiton-real"  on  real      "$SLICE" "$RUN/jiton-real"  > "$RUN/jiton-real.log"  2>&1 &  MPIDS+=($!)
  run_mode "jitoff-real" off real      "$SLICE" "$RUN/jitoff-real" > "$RUN/jitoff-real.log" 2>&1 &  MPIDS+=($!)
  run_mode "jiton-syn"   on  synthetic "$SLICE" "$RUN/jiton-syn"   > "$RUN/jiton-syn.log"   2>&1 &  MPIDS+=($!)
  run_mode "jitoff-syn"  off synthetic "$SLICE" "$RUN/jitoff-syn"  > "$RUN/jitoff-syn.log"  2>&1 &  MPIDS+=($!)
  echo "launched 4 modes concurrently (pids: ${MPIDS[*]}); logs in $RUN/<mode>.log"
  for p in "${MPIDS[@]}"; do wait "$p"; done
  echo "=== ALL-MODES SUMMARY ==="; for m in jiton-real jitoff-real jiton-syn jitoff-syn; do cat "$RUN/$m/SUMMARY.txt" 2>/dev/null; done
else
  run_mode "$JITMODE-$JDKMODE" "$JITMODE" "$JDKMODE" "$SLICE" "$RUN/$JITMODE-$JDKMODE"
fi
GT1=$(date +%s)
echo "=== TOTAL wall: $(( (GT1-GT0)/60 ))m$(( (GT1-GT0)%60 ))s | results under $RUN ==="
rm -f "$SLICE"
