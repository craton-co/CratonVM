#!/usr/bin/env bash
# =============================================================================
# run-hibernate-suite.sh — CratonVM hibernate test-suite runner
#
# Runs hibernate JUnit5 test classes on CratonVM, fork-per-class (so a
# crash/hang in one class never stops the rest), with per-class timing and full
# logs persisted to disk. Structurally this is apps/hib-suite-runner/run-hib.sh
# ported to hibernate — same @@RESULT line format (CratonRunner.java),
# same passed.txt/others.txt categorize split, same shard/timeout/override
# mechanics. Read that script's header first if this one is confusing.
#
# IMPORTANT — hibernate-specific caveat (see README.md for detail):
# almost the entire suite (everything extending BaseReactiveTest, i.e. nearly
# all of hibernate-core's tests plus all of integration-tests/) opens
# a live reactive DB connection (Postgres/MySQL/DB2/CockroachDB/... via the
# Vert.x reactive SQL client) during test setup, normally spun up through
# Testcontainers/Docker. Without a reachable DB those classes fail FAST with a
# connection-refused (or a Testcontainers "Could not find a valid Docker
# environment" error) rather than hanging — that is an environment gap, not a
# CratonVM defect, and is expected until a future session runs this with an
# actual database (see DB-REQUIRED.md for the full list / how to tell them
# apart in results.tsv). A CratonVM crash/hang on a class is still a real
# finding either way.
#
# Classes are split into two categories (see passed.txt / others.txt):
#   passed  — classes that pass on CratonVM
#   others  — everything else (fail / hang / crash / loaderror / no-db)
# =============================================================================
set -uo pipefail
export MSYS2_ARG_CONV_EXCL='*'
export MSYS_NO_PATHCONV=1

# --- locations (Windows-form paths: the VM and JVM need native paths) --------
HERE="C:/craton/CratonVM/apps/hibernate-reactive-suite-runner"
COMMON="${HR_COMMON:-$HERE/common.args}"          # -cp + sysprops for the forked VM
RUNNER_CLASS="CratonRunner"         # compiled in $HERE, already on the classpath
SELF_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" 2>/dev/null && pwd)"
OVERRIDES="${HR_CLASS_OVERRIDES:-}"
if [ -z "$OVERRIDES" ]; then
  if [ -f "$SELF_DIR/class-overrides.tsv" ]; then OVERRIDES="$SELF_DIR/class-overrides.tsv"
  else OVERRIDES="$HERE/class-overrides.tsv"; fi
fi

# --- JDK autodetect ----------------------------------------------------------
detect_jdk() {
  local c
  for c in "C:/Program Files/Eclipse Adoptium"/jdk-25* \
           "C:/Program Files/Java"/jdk-25* \
           "C:/Program Files/Eclipse Adoptium"/jdk-2* \
           "C:/Program Files/Java"/jdk-2*; do
    [ -x "$c/bin/java.exe" ] && { printf '%s' "$c"; return 0; }
  done
  return 1
}

# --- defaults (override via flags or env) ------------------------------------
CV_BIN="${CV_BIN:-C:/craton/CratonVM/target/release/cratonvm.exe}"
CV_BIN_EXPLICIT=0                   # set to 1 by --bin, so --hotspot won't clobber it
JDK="${JDK:-$(detect_jdk)}"
CV_XMX="${CV_XMX:-1500m}"
SHARDS="${SHARDS:-6}"               # parallel forks per mode
TIMEOUT="${TIMEOUT:-120}"          # per-class wall cap (s) -> HANG. hibernate
                                    # classes without a DB fail in well under a second
                                    # (connection refused); this is much shorter than
                                    # hib-suite-runner's 300s default on purpose.
OUTROOT="${OUTROOT:-$HERE/runs}"
USE_OVERRIDES=1                     # --no-overrides disables the table (A/B)
HOTSPOT=0                           # --hotspot: run against real HotSpot java.exe
HOTSPOT_HOME="${HOTSPOT_HOME:-C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot}"

CATEGORY="passed"
JITMODE="on"
JDKMODE="real"
COUNT="0"                           # 0 = all from START to end
START="0"
ALLMODES="0"
GCMODE=""                           # --gc <default|g1|zgc|generational>: adds -XX:+Use{G1,Z,Generational}GC (empty = engine default = ZGC)

usage() {
  cat <<'USAGE'
run-hibernate-reactive-suite.sh — run the hibernate-reactive suite on CratonVM

USAGE:
  run-hibernate-reactive-suite.sh [options]

OPTIONS:
  --category <passed|others>   which list to run (default: passed)
  --count <N>                  number of classes to run (default: 0 = all)
  --start <IDX>                0-based start index into the category list (default: 0)
  --jit <on|off>               JIT on, or off via --nojit (default: on)
  --jdk <real|synthetic>       real-JDK (--java-home) or synthetic (--synthetic-jdk) (default: real)
  --gc <default|g1|zgc|generational>  collector: default = engine default (ZGC), or force G1/ZGC/Generational
  --all-modes                  run all 4 modes (jit{on,off} x jdk{real,synthetic}) concurrently
  --hotspot                    run against real HotSpot java.exe instead of CratonVM, for
                               baseline comparison. Defaults --bin to the JDK 25 HotSpot
                               java.exe (unless --bin is also given) and builds VM flags as
                               plain `-Xmx<n>` — none of --java-home/--nojit/--synthetic-jdk
                               are CratonVM-only concepts and are NOT passed; --jit/--jdk are
                               ignored in this mode. Same fork/shard/timeout/results.tsv path.
  --timeout <SEC>              per-class hang timeout (default: 120); per-class
                               overrides raise this as a floor, never lower it
  --no-overrides               ignore class-overrides.tsv entirely (A/B checks)
  --shards <N>                 parallel forks per mode (default: 6)
  --bin <path>                 cratonvm.exe (default: $CV_BIN env or release build)
  --out <dir>                  output root (default: ./runs)
  --list <file>                explicit class-list file, overrides --category (used by
                               the validation batch: a small no-DB-required subset)
  -h | --help

SUB-COMMANDS:
  run-hibernate-reactive-suite.sh categorize   rebuild passed.txt / others.txt from a full run
  run-hibernate-reactive-suite.sh overrides    print the loaded per-class override table and exit

CATEGORIES (regenerate authoritatively with:  run-hibernate-reactive-suite.sh categorize):
  passed.txt  / others.txt    in this folder

PER-CLASS OVERRIDES:
  class-overrides.tsv (next to this script) raises the wall cap and/or adds VM
  flags for individual classes that the flat default misjudges. Same format as
  hib-suite-runner's table — see that file's header for the schema.

ENV PASS-THROUGH:
  Any CRATONVM_* variable in your environment is inherited by the VM, e.g.
    CRATONVM_TIER_C2_THRESHOLD=200 run-hibernate-reactive-suite.sh --count 50
  Other knobs: CV_BIN, JDK, CV_XMX, SHARDS, TIMEOUT, OUTROOT

EXAMPLES:
  # validation batch: a small explicit list, no live DB required
  run-hibernate-reactive-suite.sh --list ./validation-batch.txt --shards 2
  # first 200 passing classes, default (JIT on, real JDK)
  run-hibernate-reactive-suite.sh --category passed --count 200
  # rebuild passed.txt / others.txt from a fresh full run (needs a live DB
  # reachable via the properties in gradle.properties / -Pdb=... for anything
  # beyond connection-refused-fast-fail classes)
  run-hibernate-reactive-suite.sh categorize
USAGE
}

# --- per-class override table (identical schema/semantics to hib-suite-runner) ----
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
    --gc)       GCMODE="$2"; shift 2;;
    --all-modes) ALLMODES=1; shift;;
    --timeout)  TIMEOUT="$2"; shift 2;;
    --shards)   SHARDS="$2"; shift 2;;
    --bin)      CV_BIN="$2"; CV_BIN_EXPLICIT=1; shift 2;;
    --out)      OUTROOT="$2"; shift 2;;
    --list)     EXPLICIT_LIST="$2"; shift 2;;
    --hotspot)  HOTSPOT=1; shift;;
    -h|--help)  usage; exit 0;;
    *) echo "unknown option: $1" >&2; usage; exit 2;;
  esac
done

load_overrides
if [ "$SHOW_OVERRIDES" = 1 ]; then print_overrides; exit 0; fi

# --hotspot: point CV_BIN at real HotSpot java.exe unless the caller already
# gave an explicit --bin. Everything downstream (fork/shard/timeout/parsing)
# is unchanged — only the binary and VMFLAGS_BASE (below, in run_mode) differ.
if [ "$HOTSPOT" = 1 ] && [ "$CV_BIN_EXPLICIT" != 1 ]; then
  CV_BIN="$HOTSPOT_HOME/bin/java.exe"
fi

ORIG_PWD="$PWD"
abspath() { case "$1" in /*|[A-Za-z]:[/\\]*) printf '%s' "$1";; *) printf '%s/%s' "$ORIG_PWD" "$1";; esac; }
CV_BIN="$(abspath "$CV_BIN")"
OUTROOT="$(abspath "$OUTROOT")"
[ -n "$EXPLICIT_LIST" ] && EXPLICIT_LIST="$(abspath "$EXPLICIT_LIST")"
cd "$HERE" || { echo "ERROR: cannot cd to fixture dir: $HERE" >&2; exit 1; }

[ -f "$CV_BIN" ] || { echo "ERROR: cratonvm binary not found: $CV_BIN (set --bin or CV_BIN)" >&2; exit 1; }
[ -f "$COMMON" ] || { echo "ERROR: common.args not found: $COMMON" >&2; exit 1; }
[ -x "$JDK/bin/java.exe" ] || { echo "ERROR: real JDK not found: '${JDK:-<none detected>}' (set --jdk-home via JDK=... env)" >&2; exit 1; }
mkdir -p "$OUTROOT"
TS="$(date +%Y%m%d-%H%M%S)"

# --- fork-per-class runner for one shard -------------------------------------
# Identical shape to run-hib.sh's run_shard: one CratonVM invocation per class,
# `@@RESULT` line parsed for the tally, rc=124 (GNU `timeout`) => HANG, any
# other nonzero/missing @@RESULT => CRASH.
run_shard() {
  local LIST="$1" OUT="$2"
  mkdir -p "$OUT"
  local RAW="$OUT/raw.log" TSV="$OUT/results.tsv"
  : > "$RAW"
  printf 'idx\tclass\tstatus\tfound\tok\tfailed\taborted\tskipped\tms\tsig\n' > "$TSV"
  local idx=0 cls tmp rc
  while IFS= read -r cls; do
    [ -z "$cls" ] && continue
    local cls_to cls_fl eff_to f
    local -a eff_flags=("${VMFLAGS_BASE[@]}")
    cls_to="${CLASS_TIMEOUT_OVERRIDE[$cls]:-}"
    cls_fl="${CLASS_FLAGS_OVERRIDE[$cls]:-}"
    eff_to="$TIMEOUT"
    if [ -n "$cls_to" ] && [ "$cls_to" -gt "$TIMEOUT" ] 2>/dev/null; then eff_to="$cls_to"; fi
    if [ -n "$cls_to" ] || [ -n "$cls_fl" ]; then
      echo "[override] $cls timeout=${eff_to}s (default ${TIMEOUT}s) extra_flags=${cls_fl:--}" >> "$RAW"
      echo "[override] $cls timeout=${eff_to}s extra_flags=${cls_fl:--}" >> "$OUT/../overrides.log"
    fi
    eff_flags+=(@"$COMMON")
    # Per-class flags go AFTER the argfile, not before it.
    #
    # They used to be appended before `@$COMMON`, which made the table unable to
    # do the one thing it most needs to do: override a system property
    # common.args already sets. A later `-D` wins, so a per-class
    # `-Djunit.jupiter.execution.timeout.default=...` placed BEFORE the argfile
    # was silently overwritten by the argfile's own 120s and the override read
    # as having no effect at all -- an inert accommodation is indistinguishable
    # from a missing one. Found 2026-08-17 while raising the cap for the five
    # classes in `residual-seven-after-the-afc-fix-20260817`.
    if [ -n "$cls_fl" ]; then
      for f in $cls_fl; do
        case " ${eff_flags[*]} " in *" $f "*) ;; *) eff_flags+=("$f");; esac
      done
    fi

    tmp=$(mktemp)
    CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 timeout "$eff_to" "$CV_BIN" "${eff_flags[@]}" \
        -Dcraton.batch=1 "$RUNNER_CLASS" "$cls" >"$tmp" 2>>"$RAW"; rc=$?
    cat "$tmp" >> "$RAW"
    local rline found ok failed aborted skipped ms status sig
    rline=$(grep "^@@RESULT " "$tmp" | head -1)
    if [ -n "$rline" ]; then
      found=$(printf '%s' "$rline"|grep -o 'found=[0-9]*'|cut -d= -f2); ok=$(printf '%s' "$rline"|grep -o 'ok=[0-9]*'|cut -d= -f2)
      failed=$(printf '%s' "$rline"|grep -o 'failed=[0-9]*'|cut -d= -f2); aborted=$(printf '%s' "$rline"|grep -o 'aborted=[0-9]*'|cut -d= -f2)
      skipped=$(printf '%s' "$rline"|grep -o 'skipped=[0-9]*'|cut -d= -f2); ms=$(printf '%s' "$rline"|grep -o 'ms=[0-9]*'|cut -d= -f2)
      status=PASS
      if [ "${failed:-0}" -gt 0 ]; then status=FAIL
      elif [ "${aborted:-0}" -gt 0 ]; then status=ABORTED
      elif [ "${found:-0}" -eq 0 ]; then status=NOTESTS; fi
      sig=""
      if [ "$status" = FAIL ]; then
        # A DB-required class failing fast with connection-refused is a
        # different finding than a CratonVM defect — flag it distinctly so
        # results.tsv doesn't conflate "no DB reachable" with "VM bug".
        # Only check $tmp (this class's own output), never $RAW (the
        # shard's cumulative log) -- otherwise an earlier class's real
        # Docker failure mislabels every later class in the same shard as
        # NO-DB regardless of its own actual outcome.
        if grep -qm1 -E "Connection refused|Could not find a valid Docker|ConnectException|No Docker environment" "$tmp" 2>/dev/null; then
          sig="NO-DB: $(grep -m1 -E "Connection refused|Could not find a valid Docker|ConnectException|No Docker environment" "$tmp" 2>/dev/null | head -c 140)"
        else
          sig=$(grep -m1 -E "^(MethodSource|[A-Za-z][A-Za-z0-9_.]*(Exception|Error))" "$tmp" | head -c 160)
        fi
      fi
      printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$idx" "$cls" "$status" "${found:-0}" "${ok:-0}" "${failed:-0}" "${aborted:-0}" "${skipped:-0}" "${ms:-0}" "$sig" >> "$TSV"
    else
      local st; if [ "$rc" -eq 124 ]; then st=HANG; else st=CRASH; fi
      printf '%s\t%s\t%s\t0\t0\t0\t0\t0\t0\t%s rc=%s timeout=%ss\n' "$idx" "$cls" "$st" "process-died" "$rc" "$eff_to" >> "$TSV"
    fi
    rm -f "$tmp"
    idx=$((idx+1))
  done < "$LIST"
}

# --- run one mode (sharded) --------------------------------------------------
run_mode() {
  local label="$1" jit="$2" jdk="$3" SLICE="$4" MODE="$5"
  mkdir -p "$MODE"
  if [ "$HOTSPOT" = 1 ]; then
    # Real java.exe: no --java-home/--nojit/--synthetic-jdk (CratonVM-only
    # concepts). -Xmx takes its value concatenated, e.g. -Xmx1500m.
    VMFLAGS_BASE=(-Xmx"$CV_XMX")
  else
    VMFLAGS_BASE=(--java-home "$JDK" --Xmx "$CV_XMX")
    [ "$jit" = off ] && VMFLAGS_BASE+=(--nojit)
    [ "$jdk" = synthetic ] && VMFLAGS_BASE+=(--synthetic-jdk)
    case "$GCMODE" in
      g1)          VMFLAGS_BASE+=(-XX:+UseG1GC);;
      zgc)         VMFLAGS_BASE+=(-XX:+UseZGC);;
      generational) VMFLAGS_BASE+=(-XX:+UseGenerationalGC);;
      default|"") ;;
      *) echo "ERROR: unknown --gc mode: $GCMODE (want default|g1|zgc|generational)" >&2; exit 2;;
    esac
  fi
  local n; n=$(grep -c '' "$SLICE")
  echo "[$label] $n classes | jit=$jit jdk=$jdk shards=$SHARDS timeout=${TIMEOUT}s overrides=$OVERRIDES_STATE bin=$CV_BIN hotspot=$HOTSPOT"
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
echo "=== run-hibernate-reactive-suite $TS :: category=$CATEGORY start=$START count=$COUNT -> $SLN classes ==="

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
