#!/usr/bin/env bash
# =============================================================================
# run-quarkus-suite.sh — CratonVM Quarkus core test-suite runner (Linux / Azure host)
#
# Runs Quarkus's JUnit 5 (Jupiter) test classes on CratonVM, fork-per-class (so a
# crash/hang in one class never stops the rest), with per-class timing and full
# logs persisted to disk.
#
# Same shape as apps/hib-suite-runner/run-hib.sh and the Windows-side
# apps/quarkus-suite-runner (see that dir's README for the prior Windows-only
# validation run):
#   - classes split into passed.txt (passes on CratonVM) / others.txt (everything
#     else: fail / hang / crash / loaderror)
#   - fork-per-class via CratonRunner (JUnit Platform Launcher), one VM per class
#   - sharded across parallel forks, with a flat per-class wall-clock timeout
#   - `categorize` sub-command rebuilds passed.txt/others.txt from a full run
#   - per-class accommodations via class-overrides.tsv (tracked in git)
#
# CratonRunner takes class names directly as argv (one VM per class here, for
# crash/hang isolation) and prints
#   @@RESULT <className> found=.. started=.. ok=.. failed=.. aborted=.. skipped=.. ms=..
# plus @@BATCHEND failed_classes=N.
#
# This is a CratonVM-only harness (no --hotspot baseline mode) for this phase.
#
# Quarkus specifics on this host:
#   - Quarkus's full reactor is ~700 modules (nearly every extension); this
#     harness deliberately does NOT attempt that. It is scoped to `core/*`
#     (deployment, runtime, processor, builder, devmode-spi, launcher,
#     class-change-agent) plus their `-am` dependency closure, built via
#     `./mvnw -pl <core modules> -am install -Dquickly`. This built and
#     installed cleanly (372/372 goals, ~45s).
#   - testlist.txt (82 classes) covers only these 7 modules' own tests —
#     most are plain unit tests of Quarkus's build-time/deployment machinery,
#     not `@QuarkusTest`-annotated application tests.
#   - Quarkus's `@QuarkusTest` integration-test style (used pervasively outside
#     core/*) needs Quarkus's own application bootstrap (a running augmented
#     application, config resolution, CDI container, etc.) that a flat
#     classpath + JUnit Platform Launcher harness cannot provide. Those
#     classes are structurally out of reach for this harness and are expected
#     to report NOTESTS or hang/crash during bootstrap — that is a harness
#     scope limitation, not automatically a CratonVM defect finding.
#   - common.args' -cp is `<module>/target/classes` + `<module>/target/test-classes`
#     across the 7 core modules, plus every jar `dependency:build-classpath
#     -DincludeScope=test` resolved per module (see README.md for how it was built).
# =============================================================================
set -uo pipefail

# --- locations -----------------------------------------------------------
HERE="/data/cratonvm/apps/quarkus-suite-runner"
COMMON="$HERE/common.args"          # -cp + sysprops for the forked VM
RUNNER_CLASS="CratonRunner"         # compiled in $HERE, already on the classpath
SELF_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" 2>/dev/null && pwd)"
OVERRIDES="${QUARKUS_CLASS_OVERRIDES:-}"
if [ -z "$OVERRIDES" ]; then
  if [ -f "$SELF_DIR/class-overrides.tsv" ]; then OVERRIDES="$SELF_DIR/class-overrides.tsv"
  else OVERRIDES="$HERE/class-overrides.tsv"; fi
fi

# --- defaults (override via flags or env) ---------------------------------
CV_BIN="${CV_BIN:-/data/cratonvm/target/release/cratonvm}"
JDK="${JDK:-/data/toolchain/jdk-25}"
CV_XMX="${CV_XMX:-1500m}"
SHARDS="${SHARDS:-6}"               # parallel forks per mode
TIMEOUT="${TIMEOUT:-180}"           # per-class wall cap (s) -> HANG
OUTROOT="${OUTROOT:-$HERE/runs}"
USE_OVERRIDES=1                     # --no-overrides disables the table (A/B)

CATEGORY="passed"
JITMODE="on"
JDKMODE="real"
COUNT="0"                           # 0 = all from START to end
START="0"
ALLMODES="0"

usage() {
  cat <<'USAGE'
run-quarkus-suite.sh — run the Quarkus core test suite on CratonVM

USAGE:
  run-quarkus-suite.sh [options]

OPTIONS:
  --category <passed|others>   which list to run (default: passed)
  --count <N>                  number of classes to run (default: 0 = all)
  --start <IDX>                0-based start index into the category list (default: 0)
  --jit <on|off>               JIT on, or off via --nojit (default: on)
  --jdk <real|synthetic>       real-JDK (--java-home) or synthetic (--synthetic-jdk) (default: real)
  --all-modes                  run all 4 modes (jit{on,off} x jdk{real,synthetic}) concurrently,
                               each in its own background thread (ignores --jit/--jdk)
  --timeout <SEC>              per-class hang timeout (default: 180); per-class
                               overrides raise this as a floor, never lower it
  --no-overrides               ignore class-overrides.tsv entirely (A/B checks)
  --shards <N>                 parallel forks per mode (default: 6)
  --bin <path>                 cratonvm binary (default: $CV_BIN env or release build)
  --out <dir>                  output root (default: ./runs)
  --list <file>                run an explicit class-list file instead of a category
  -h | --help

SUB-COMMANDS:
  run-quarkus-suite.sh categorize   rebuild passed.txt / others.txt from a full run
  run-quarkus-suite.sh overrides    print the loaded per-class override table and exit

CATEGORIES (regenerate authoritatively with:  run-quarkus-suite.sh categorize):
  passed.txt  / others.txt    in this folder

PER-CLASS OVERRIDES:
  class-overrides.tsv (tracked in git, next to this script) raises the wall cap
  and/or adds VM flags for individual classes that the flat default misjudges.
  Every run prints `overrides=N (loaded)` in its mode header; `overrides=0
  (MISSING ...)` means the table is gone. Check it with `run-quarkus-suite.sh overrides`.

ENV PASS-THROUGH:
  Any CRATONVM_* variable in your environment is inherited by the VM, e.g.
    CRATONVM_TIER_C2_THRESHOLD=200 run-quarkus-suite.sh --count 20
  Other knobs: CV_BIN, JDK, CV_XMX, SHARDS, TIMEOUT, OUTROOT

EXAMPLES:
  # small validation batch, first 15 classes of testlist.txt
  run-quarkus-suite.sh --list testlist.txt --count 15
  # first 200 passing classes, default (JIT on, real JDK)
  run-quarkus-suite.sh --category passed --count 200
  # rebuild passed.txt / others.txt from a fresh full run over testlist.txt
  run-quarkus-suite.sh categorize
USAGE
}

# --- per-class override table ------------------------------------------------
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
  while IFS=$'\t' read -r cls to fl _rest || [ -n "${cls:-}" ]; do
    cls="${cls%$'\r'}"; to="${to:-}"; to="${to%$'\r'}"; fl="${fl:-}"; fl="${fl%$'\r'}"
    case "$cls" in ''|\#*) continue;; esac
    if [ -n "$to" ] && [ "$to" != "-" ]; then
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

# --- categorize sub-command: (re)build passed.txt / others.txt ----------------
if [ "${1:-}" = "categorize" ]; then
  echo "[categorize] running the full testlist once (JIT on, real JDK) to split passed/others ..."
  CATEGORIZE=1
else
  CATEGORIZE=0
fi

# --- arg parse ---------------------------------------------------------------
SHOW_OVERRIDES=0
EXPLICIT_LIST=""
while [ $# -gt 0 ]; do
  case "$1" in
    categorize) shift;;
    overrides)  SHOW_OVERRIDES=1; shift;;
    --no-overrides) USE_OVERRIDES=0; shift;;
    --category) CATEGORY="$2"; shift 2;;
    --count)    COUNT="$2"; shift 2;;
    --start)    START="$2"; shift 2;;
    --jit)      JITMODE="$2"; shift 2;;
    --jdk)      JDKMODE="$2"; shift 2;;
    --all-modes) ALLMODES=1; shift;;
    --timeout)  TIMEOUT="$2"; shift 2;;
    --shards)   SHARDS="$2"; shift 2;;
    --bin)      CV_BIN="$2"; shift 2;;
    --out)      OUTROOT="$2"; shift 2;;
    --list)     EXPLICIT_LIST="$2"; shift 2;;
    -h|--help)  usage; exit 0;;
    *) echo "unknown option: $1" >&2; usage; exit 2;;
  esac
done

load_overrides
if [ "$SHOW_OVERRIDES" = 1 ]; then print_overrides; exit 0; fi

ORIG_PWD="$PWD"
abspath() { case "$1" in /*) printf '%s' "$1";; *) printf '%s/%s' "$ORIG_PWD" "$1";; esac; }
CV_BIN="$(abspath "$CV_BIN")"
OUTROOT="$(abspath "$OUTROOT")"
[ -n "$EXPLICIT_LIST" ] && EXPLICIT_LIST="$(abspath "$EXPLICIT_LIST")"
cd "$HERE" || { echo "ERROR: cannot cd to fixture dir: $HERE" >&2; exit 1; }

[ -f "$CV_BIN" ] || { echo "ERROR: cratonvm binary not found: $CV_BIN (set --bin or CV_BIN)" >&2; exit 1; }
[ -f "$COMMON" ] || { echo "ERROR: common.args not found: $COMMON" >&2; exit 1; }
[ -x "$JDK/bin/java" ] || { echo "ERROR: real JDK not found: '${JDK:-<none detected>}' (set JDK=... env)" >&2; exit 1; }
mkdir -p "$OUTROOT"
TS="$(date +%Y%m%d-%H%M%S)"

# --- fork-per-class runner for one shard -------------------------------------
run_shard() {
  local LIST="$1" OUT="$2"
  mkdir -p "$OUT"
  local RAW="$OUT/raw.log" TSV="$OUT/results.tsv"
  : > "$RAW"
  printf 'idx\tclass\tstatus\tfound\tok\tfailed\taborted\tskipped\tms\tsig\tstarted\n' > "$TSV"
  local idx=0 cls tmp rc
  while IFS= read -r cls; do
    [ -z "$cls" ] && continue
    local cls_to cls_fl eff_to f
    local -a eff_flags=("${VMFLAGS_BASE[@]}")
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
    local rline found ok failed aborted skipped ms status sig started
    rline=$(grep "^@@RESULT " "$tmp" | head -1)
    if [ -n "$rline" ]; then
      found=$(printf '%s' "$rline"|grep -o 'found=[0-9]*'|cut -d= -f2); ok=$(printf '%s' "$rline"|grep -o 'ok=[0-9]*'|cut -d= -f2)
      failed=$(printf '%s' "$rline"|grep -o 'failed=[0-9]*'|cut -d= -f2); aborted=$(printf '%s' "$rline"|grep -o 'aborted=[0-9]*'|cut -d= -f2)
      skipped=$(printf '%s' "$rline"|grep -o 'skipped=[0-9]*'|cut -d= -f2); ms=$(printf '%s' "$rline"|grep -o 'ms=[0-9]*'|cut -d= -f2)
      started=$(printf '%s' "$rline"|grep -o 'started=[0-9]*'|cut -d= -f2)
      status=PASS
      if [ "${failed:-0}" -gt 0 ]; then status=FAIL
      elif [ "${aborted:-0}" -gt 0 ]; then status=ABORTED
      elif [ "${found:-0}" -eq 0 ]; then status=NOTESTS
      elif [ "${started:-0}" -eq 0 ]; then status=NOSTART; fi
      sig=""
      if [ "$status" = FAIL ]; then sig=$(grep -m1 -E "^(MethodSource|[A-Za-z][A-Za-z0-9_.]*(Exception|Error))" "$tmp" | head -c 160); fi
      if [ "$status" = NOSTART ]; then sig=$(grep -m1 -E "^\[.*\]\s*(NoSuchMethodError|NoSuchFieldError|AbstractMethodError|LinkageError|ExceptionInInitializerError)" "$tmp" | head -c 160); fi
      printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$idx" "$cls" "$status" "${found:-0}" "${ok:-0}" "${failed:-0}" "${aborted:-0}" "${skipped:-0}" "${ms:-0}" "$sig" "${started:-0}" >> "$TSV"
    else
      local st; if [ "$rc" -eq 124 ]; then st=HANG; else st=CRASH; fi
      printf '%s\t%s\t%s\t0\t0\t0\t0\t0\t0\t%s rc=%s timeout=%ss\t0\n' "$idx" "$cls" "$st" "process-died" "$rc" "$eff_to" >> "$TSV"
    fi
    rm -f "$tmp"
    idx=$((idx+1))
  done < "$LIST"
}

# --- run one mode (sharded) --------------------------------------------------
run_mode() {
  local label="$1" jit="$2" jdk="$3" SLICE="$4" MODE="$5"
  mkdir -p "$MODE"
  VMFLAGS_BASE=(--java-home "$JDK" --Xmx "$CV_XMX")
  [ "$jit" = off ] && VMFLAGS_BASE+=(--nojit)
  [ "$jdk" = synthetic ] && VMFLAGS_BASE+=(--synthetic-jdk)
  local n; n=$(grep -c '' "$SLICE")
  echo "[$label] $n classes | jit=$jit jdk=$jdk shards=$SHARDS timeout=${TIMEOUT}s overrides=$OVERRIDES_STATE bin=$CV_BIN"
  local t0; t0=$(date +%s)
  local s pids=()
  for ((s=0; s<SHARDS; s++)); do awk -v n="$SHARDS" -v r="$s" 'NR%n==r' "$SLICE" > "$MODE/shard-$s.txt"; done
  for ((s=0; s<SHARDS; s++)); do ( run_shard "$MODE/shard-$s.txt" "$MODE/shard-$s" ) & pids+=($!); done
  for p in "${pids[@]}"; do wait "$p"; done
  local t1; t1=$(date +%s); local secs=$((t1-t0))
  local MERGED="$MODE/results.tsv"
  head -1 "$MODE/shard-0/results.tsv" > "$MERGED" 2>/dev/null
  for ((s=0; s<SHARDS; s++)); do tail -n +2 "$MODE/shard-$s/results.tsv" 2>/dev/null; done >> "$MERGED"
  local rec; rec=$(( $(grep -c '' "$MERGED") - 1 ))
  {
    echo "mode=$label jit=$jit jdk=$jdk classes=$n recorded=$rec wall_seconds=$secs ($((secs/60))m$((secs%60))s) overrides=$OVERRIDES_STATE"
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
echo "=== run-quarkus-suite $TS :: category=$CATEGORY start=$START count=$COUNT -> $SLN classes ==="

# --- categorize: run full list (jit on/real), then split ---------------------
if [ "$CATEGORIZE" = 1 ]; then
  RUN="$OUTROOT/categorize-$TS"
  run_mode "categorize" on real "$SLICE" "$RUN"
  awk -F'\t' 'NR>1 && $3=="PASS"{print $2}' "$RUN/results.tsv" | sort -u > "$HERE/passed.txt"
  awk -F'\t' 'NR>1 && $3!="PASS"{print $2}' "$RUN/results.tsv" | sort -u > "$HERE/others.txt"
  echo "rebuilt passed.txt=$(grep -c '' "$HERE/passed.txt")  others.txt=$(grep -c '' "$HERE/others.txt")"
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
