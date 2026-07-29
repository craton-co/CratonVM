#!/usr/bin/env bash
###############################################################################
# run-suite.sh — Spring Framework test-suite driver for CratonVM
#
# Runs the apps/spring-framework JUnit test classes under cratonvm.exe, split
# into two categories (PASSED / OTHERS), with selectable JIT and JDK modes,
# per-class timing, full log persistence, and a 4-modes-at-once parallel runner.
#
# Quick examples (run from anywhere; Git Bash):
#   ./run-suite.sh discover                 # build the master class index
#   ./run-suite.sh categorize               # baseline run -> passed/others lists
#   ./run-suite.sh run --category passed --count 100          # 100 passed, mode jit-real
#   ./run-suite.sh run --category failed --start 200 --count 50 --jit off
#   ./run-suite.sh run --category passed --jdk synthetic --jit off
#   ./run-suite.sh quad --category passed --count 1000        # all 4 modes in parallel
#   ./run-suite.sh hotspot --category passed --count 1000     # HotSpot baseline timing
#
# See README.md for the full guide.
###############################################################################
set -u
export MSYS2_ARG_CONV_EXCL='*'; export MSYS_NO_PATHCONV=1

# Portable cygpath shim: on Linux there is no cygpath, and paths are already
# native.  Passing them through unchanged is exactly right there.
if ! command -v cygpath >/dev/null 2>&1; then
  cygpath() { local a; for a in "$@"; do case "$a" in -*) ;; *) printf '%s' "$a";; esac; done; echo; }
fi

# ---------------------------------------------------------------- locations ---
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SPRING="${SPRING:-/c/craton/cratonvm/apps/spring-framework}"
OUTROOT="${OUTROOT:-$HERE/out}"
META="$HERE/meta"                       # class index + category lists live here
mkdir -p "$OUTROOT" "$META"

ALLIDX="$META/all-classes.tsv"          # module<TAB>fqcn   (stable, sorted)
PASSED="$META/passed.tsv"               # subset: baseline status==OK
OTHERS="$META/others.tsv"               # all-classes minus passed

# HotSpot JDK 25 (real-JDK mode + javac for KRun + the `hotspot` baseline).
JDK25="${JDK25:-/c/Program Files/Microsoft/jdk-25.0.3.9-hotspot}"
JDK25_WIN="${JDK25_WIN:-C:\\Program Files\\Microsoft\\jdk-25.0.3.9-hotspot}"

# cratonvm.exe — env override, else common build locations.
find_vm() {
  local c
  for c in "${CRATONVM_BIN:-}" \
           "/c/craton/cratonvm/target/release/cratonvm.exe" \
           "/c/craton/cratonvm/target/debug/cratonvm.exe" \
           "/c/craton/CratonVM/target/release/cratonvm.exe"; do
    [ -n "$c" ] && [ -x "$c" ] && { echo "$c"; return 0; }
  done
  return 1
}

# Tunables (env or flags).
# A fresh VM per class is the correctness default: Spring tests create global
# instrumentation and compiler/class-loader state that is unsafe to batch.
BATCH="${BATCH:-1}"             # classes per cratonvm.exe invocation
BATCH_TO="${BATCH_TO:-600}"     # per-batch timeout (s) — hang detector
ONE_TO="${ONE_TO:-180}"         # per-class re-run timeout (s) for crash recovery
EXTRA_VM_ARGS="${EXTRA_VM_ARGS:-}"   # extra cratonvm CLI args (verbatim)

# ------------------------------------------------------------------ helpers ---
die() { echo "ERROR: $*" >&2; exit 1; }
log() { echo "[$(date +%H:%M:%S)] $*"; }

# Priority so the index order (and therefore --start indices) is stable and
# fast/high-signal core modules come first.
PRIO="spring-core spring-beans spring-expression spring-aop spring-context spring-tx spring-jdbc spring-messaging spring-web spring-jms spring-orm spring-oxm spring-r2dbc spring-context-support spring-aspects spring-webflux spring-websocket spring-webmvc spring-core-test spring-context-indexer spring-instrument spring-test framework-docs integration-tests"
rank() { local b="$1" i=1 p; for p in $PRIO; do [ "$p" = "$b" ] && { printf '%03d' "$i"; return; }; i=$((i+1)); done; echo 050; }

compile_krun() {
  [ -f "$HERE/KRun.class" ] && return 0
  log "compiling KRun.java (HotSpot javac)"
  local cpf="$SPRING/spring-core/build/cratonvm-testcp.txt"
  [ -f "$cpf" ] || die "no spring-core testcp for compiling KRun: $cpf (build the suite first)"
  local cp; cp="$(tr -d '\r' < "$cpf")"
  "$JDK25/bin/javac.exe" -cp "$cp" -d "$(cygpath -w "$HERE")" "$(cygpath -w "$HERE/KRun.java")" \
    || die "KRun.java failed to compile"
}

# ------------------------------------------------------------- discover idx ---
discover() {
  log "discovering test classes under $SPRING ..."
  mapfile -t TESTDIRS < <(find "$SPRING" -type d -path '*/build/classes/*/test' 2>/dev/null | sort -u)
  declare -A MODS
  for td in "${TESTDIRS[@]}"; do MODS["${td%/build/classes/*}"]=1; done
  : > "$ALLIDX.tmp"
  for MOD in "${!MODS[@]}"; do
    local MODNAME; MODNAME="$(basename "$MOD")"
    [ -f "$MOD/build/cratonvm-testcp.txt" ] || { log "skip $MODNAME (no cratonvm-testcp.txt)"; continue; }
    local R; R="$(rank "$MODNAME")"
    for d in "$MOD"/build/classes/*/test; do
      [ -d "$d" ] || continue
      (cd "$d" && find . \( -name '*Tests.class' -o -name '*Test.class' \) ! -name '*$*' \
         | sed 's|^\./||; s|\.class$||; s|/|.|g')
    done | sort -u | while read -r cls; do
      printf '%s\t%s\t%s\n' "$R" "$MOD" "$cls"
    done >> "$ALLIDX.tmp"
  done
  # stable order: rank, module, class — then strip the rank column.
  # The `*Tests.class` filename filter above also matches `Abstract*Tests` base
  # classes (and JUnit meta-annotation interfaces such as
  # `@PathPatternsParameterizedTest`).  Those are not runnable standalone —
  # 0 tests is the correct result for them on any JVM — so indexing them just
  # inflated every run's EMPTY/non-passed count.  Drop them by checking
  # ACC_ABSTRACT/ACC_INTERFACE in the class file itself.
  sort -u "$ALLIDX.tmp" | sort -s -k1,1 | cut -f2,3 > "$ALLIDX.all"
  if command -v python3 >/dev/null 2>&1 && [ -f "$HERE/is_concrete.py" ]; then
    python3 "$HERE/is_concrete.py" < "$ALLIDX.all" > "$ALLIDX"
  else
    log "WARN: python3/is_concrete.py unavailable — keeping abstract classes in the index"
    cp "$ALLIDX.all" "$ALLIDX"
  fi
  rm -f "$ALLIDX.tmp"
  log "indexed $(wc -l < "$ALLIDX") test classes -> $ALLIDX"
}

ensure_idx() { [ -s "$ALLIDX" ] || discover; }

# --------------------------------------------------------------- categorize ---
# Build passed.tsv / others.tsv from a results.tsv. If none given, run the
# canonical baseline mode (jit-real) over the whole suite first.
categorize() {
  ensure_idx
  local results="${1:-}"
  if [ -z "$results" ]; then
    log "no results.tsv given — running canonical baseline (jit-real) over the whole index"
    run_mode "jit-real" "$ALLIDX" 0 0 "$OUTROOT/baseline-jit-real"
    results="$OUTROOT/baseline-jit-real/results.tsv"
  fi
  [ -s "$results" ] || die "results file empty/missing: $results"
  # passed = status OK in results
  awk -F'\t' '$2=="OK"{print $1}' "$results" | sort -u > "$META/.passed.classes"
  # passed.tsv keeps module<TAB>class rows, in index order
  awk -F'\t' 'NR==FNR{ok[$1]=1; next} ($2 in ok){print}' "$META/.passed.classes" "$ALLIDX" > "$PASSED"
  awk -F'\t' 'NR==FNR{ok[$1]=1; next} !($2 in ok){print}' "$META/.passed.classes" "$ALLIDX" > "$OTHERS"
  rm -f "$META/.passed.classes"
  log "passed=$(wc -l < "$PASSED")  others=$(wc -l < "$OTHERS")  (from $results)"
}

# ----------------------------------------------------------- one mode runner ---
# run_mode <mode> <listfile> <start> <count> <outdir>
#   mode: jit-real | nojit-real | jit-syn | nojit-syn | hotspot
#   start: 1-based starting index into listfile (0 = from beginning)
#   count: number of classes (0 = all)
run_mode() {
  local mode="$1" listfile="$2" start="$3" count="$4" outdir="$5"
  mkdir -p "$outdir"
  local RES="$outdir/results.tsv" RAW="$outdir/raw.log" CRASH="$outdir/crashes.log"
  local FC="$outdir/failcauses.log" RUN="$outdir/run.log" TIMING="$outdir/timing.tsv"
  : > "$RES"; : > "$RAW"; : > "$CRASH"; : > "$FC"; : > "$RUN"; : > "$TIMING"

  # mode -> VM/binary + flags. STACK_ARGS is cratonvm-only (java.exe rejects it).
  local VM JH_ARGS=() JIT_ARGS=() label="$mode"
  local STACK_ARGS=(--stack-dump-on-timeout 0)
  case "$mode" in
    hotspot)   VM="$JDK25/bin/java.exe"; STACK_ARGS=() ;;
    jit-real)  VM="$(find_vm)" || die "cratonvm.exe not found (set CRATONVM_BIN or build it)"; JH_ARGS=(--java-home "$JDK25_WIN") ;;
    nojit-real)VM="$(find_vm)" || die "cratonvm.exe not found"; JH_ARGS=(--java-home "$JDK25_WIN"); JIT_ARGS=(--nojit) ;;
    jit-syn)   VM="$(find_vm)" || die "cratonvm.exe not found"; JH_ARGS=(--synthetic-jdk) ;;
    nojit-syn) VM="$(find_vm)" || die "cratonvm.exe not found"; JH_ARGS=(--synthetic-jdk); JIT_ARGS=(--nojit) ;;
    *) die "unknown mode: $mode" ;;
  esac

  # slice the list
  local sliced; sliced="$(mktemp)"
  awk -v s="$start" -v c="$count" '
    BEGIN { first=(s>0?s:1) }
    NR>=first { print; n++; if (c>0 && n>=c) exit }
  ' "$listfile" > "$sliced"
  local total; total="$(wc -l < "$sliced")"
  {
    echo "mode=$label  binary=$VM"
    echo "extra_vm_args=$EXTRA_VM_ARGS  jit_args=${JIT_ARGS[*]:-}  jdk_args=${JH_ARGS[*]:-}"
    echo "list=$listfile  start=$start  count=$count  -> $total classes"
    echo "batch=$BATCH  batch_to=${BATCH_TO}s  one_to=${ONE_TO}s"
    echo "started=$(date '+%F %T')"
  } | tee -a "$RUN"
  [ "$total" -eq 0 ] && { echo "nothing to run"; rm -f "$sliced"; return 0; }

  local KRUN_W; KRUN_W="$(cygpath -m "$HERE")"
  local mode_t0; mode_t0=$(date +%s)

  # group consecutive rows by module (column 1) so each batch shares a classpath
  local cur_mod="" af="" afm=""
  local -a batch=()
  flush_batch() {
    [ "${#batch[@]}" -eq 0 ] && return
    local raw rc
    raw=$(cd "$cur_mod" && timeout "$BATCH_TO" "$VM" "${JIT_ARGS[@]}" "${JH_ARGS[@]}" $EXTRA_VM_ARGS \
            "${STACK_ARGS[@]}" "@$afm" KRun "${batch[@]}" 2>"$outdir/.err"); rc=$?
    printf '%s\n' "$raw" >> "$RAW"
    printf '%s\n' "$raw" | grep -E '^FAILCAUSE|^LOADERR' >> "$FC"
    mapfile -t got < <(printf '%s\n' "$raw" | sed -n 's/^RESULT \([^ ]*\) .*/\1/p')
    record_results "$raw" "$RES" "$TIMING" "$label"
    if [ $rc -ne 0 ]; then
      { echo "===== BATCH [$cur_mod rc=$rc] ====="; printf '%s\n' "$raw" | tail -8; echo "--err--"; tail -15 "$outdir/.err"; echo; } >> "$CRASH"
    fi
    # crash recovery: any class with no RESULT gets a lone re-run
    local cls
    for cls in "${batch[@]}"; do
      printf '%s\n' "${got[@]}" | grep -qxF "$cls" && continue
      run_one_class "$cls" "$afm" "$VM" "$RES" "$TIMING" "$RAW" "$CRASH" "$FC" "$label" "$cur_mod" \
                    "${JIT_ARGS[@]:-}" "::" "${JH_ARGS[@]:-}"
    done
    batch=()
  }

  local n_done=0 mod cls
  while IFS=$'\t' read -r mod cls; do
    if [ "$mod" != "$cur_mod" ]; then
      flush_batch
      cur_mod="$mod"
      local cpf="$mod/build/cratonvm-testcp.txt"
      # Match Gradle's test runtime instead of preferring the packaged JAR.
      local mcp="$KRUN_W:$mod/build/classes/java/main:$mod/build/classes/kotlin/main:$mod/build/resources/main:$(tr -d '\r' < "$cpf")"
      af="$outdir/.af_$(basename "$mod").txt"; { echo "-cp"; echo "$mcp"; } > "$af"
      afm="$(cygpath -m "$af")"
    fi
    batch+=("$cls")
    n_done=$((n_done+1))
    if [ "${#batch[@]}" -ge "$BATCH" ]; then
      flush_batch
      printf '[%s] %d/%d\n' "$label" "$n_done" "$total" | tee -a "$RUN"
    fi
  done < "$sliced"
  flush_batch
  rm -f "$sliced"

  local mode_t1; mode_t1=$(date +%s)
  local wall=$((mode_t1 - mode_t0))
  {
    echo "finished=$(date '+%F %T')  wall=${wall}s"
    awk -F'\t' '{c[$2]++; f+=$3; s+=$4; x+=$5; t+=$8}
       END{ printf "classes:"; for(k in c) printf " %s=%d", k, c[k];
            printf "\ntest-methods: found=%d passed=%d failed=%d   sum-class-ms=%d\n", f, s, x, t }' "$RES"
    echo "wall-clock=${wall}s"
  } | tee -a "$RUN" | tee "$outdir/summary.txt"
  log "[$label] done in ${wall}s -> $outdir"
}

# parse KRun stdout -> results.tsv (class status found succ fail skip abort ms mode)
record_results() {
  local raw="$1" res="$2" timing="$3" mode="$4"
  printf '%s\n' "$raw" | awk -v mode="$mode" -v tf="$timing" '
    /^RESULT / { c=$2; delete v; for(i=3;i<=NF;i++){split($i,a,"=");v[a[1]]=a[2]}
       printf "%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n", c, v["status"], v["found"]+0, v["succ"]+0, v["fail"]+0, v["skip"]+0, v["abort"]+0, v["ms"]+0, mode
       print c"\t"v["ms"]+0"\t"v["status"]"\t"mode >> tf }' >> "$res"
}

# lone re-run of a single class (crash recovery / timeout isolation)
run_one_class() {
  local cls="$1" afm="$2" VM="$3" RES="$4" TIMING="$5" RAW="$6" CRASH="$7" FC="$8" mode="$9" curmod="${10}"
  shift 10
  # remaining args: JIT_ARGS... "::" JH_ARGS...
  local -a JITA=() JHA=(); local seen=0 a
  for a in "$@"; do
    if [ "$a" = "::" ]; then seen=1; continue; fi
    [ -z "$a" ] && continue
    if [ $seen -eq 0 ]; then JITA+=("$a"); else JHA+=("$a"); fi
  done
  local raw rc line
  local -a SA=(--stack-dump-on-timeout 0); [ "$mode" = "hotspot" ] && SA=()
  raw=$(cd "$curmod" && timeout "$ONE_TO" "$VM" "${JITA[@]}" "${JHA[@]}" $EXTRA_VM_ARGS \
          "${SA[@]}" "@$afm" KRun "$cls" 2>"$(dirname "$RES")/.err1"); rc=$?
  printf '%s\n' "$raw" >> "$RAW"
  printf '%s\n' "$raw" | grep -E '^FAILCAUSE|^LOADERR' >> "$FC"
  line=$(printf '%s\n' "$raw" | grep -m1 '^RESULT ')
  if [ $rc -eq 124 ]; then
    printf '%s\tTIMEOUT\t0\t0\t0\t0\t0\t%d\t%s\n' "$cls" $((ONE_TO*1000)) "$mode" >> "$RES"
    printf '%s\t%d\tTIMEOUT\t%s\n' "$cls" $((ONE_TO*1000)) "$mode" >> "$TIMING"
    { echo "===== $cls [TIMEOUT] ====="; printf '%s\n' "$raw" | tail -10; echo; } >> "$CRASH"
  elif [ -n "$line" ]; then
    record_results "$raw" "$RES" "$TIMING" "$mode"
  else
    local kind=CRASH
    grep -qaiE "panicked at|not yet implemented|unreachable|index out of bounds|EXCEPTION_ACCESS|STATUS_|fatal runtime" "$(dirname "$RES")/.err1" || kind=ABEND
    printf '%s\t%s\t0\t0\t0\t0\t0\t0\t%s\n' "$cls" "$kind" "$mode" >> "$RES"
    printf '%s\t0\t%s\t%s\n' "$cls" "$kind" "$mode" >> "$TIMING"
    { echo "===== $cls [$kind rc=$rc] ====="; printf '%s\n' "$raw" | tail -10; echo "--err--"; tail -15 "$(dirname "$RES")/.err1"; echo; } >> "$CRASH"
  fi
}

# --------------------------------------------------- arg parsing for run/quad ---
LISTFILE=""; CATEGORY="all"; JIT="on"; JDK="real"; START=0; COUNT=0; TAG=""; ONLY=""; SHARD=""
parse_run_args() {
  while [ $# -gt 0 ]; do
    case "$1" in
      --category) CATEGORY="$2"; shift 2 ;;
      --list)     LISTFILE="$2"; CATEGORY="custom"; shift 2 ;;
      --jit)      JIT="$2"; shift 2 ;;        # on|off
      --jdk)      JDK="$2"; shift 2 ;;        # real|synthetic
      --start)    START="$2"; shift 2 ;;
      --count)    COUNT="$2"; shift 2 ;;
      --only)     ONLY="$2"; shift 2 ;;       # regex on fully-qualified class name
      --shard)    SHARD="$2"; shift 2 ;;      # i/m round-robin shard (1-based i)
      --batch)    BATCH="$2"; shift 2 ;;
      --batch-to) BATCH_TO="$2"; shift 2 ;;
      --one-to)   ONE_TO="$2"; shift 2 ;;
      --tag)      TAG="$2"; shift 2 ;;
      *) die "unknown option: $1" ;;
    esac
  done
}

list_for_category() {
  local base
  case "$CATEGORY" in
    passed) [ -s "$PASSED" ] || die "passed.tsv missing — run: $0 categorize"; base="$PASSED" ;;
    failed|others) [ -s "$OTHERS" ] || die "others.tsv missing — run: $0 categorize"; base="$OTHERS" ;;
    all) ensure_idx; base="$ALLIDX" ;;
    custom) [ -s "$LISTFILE" ] || die "--list file missing/empty: $LISTFILE"; base="$LISTFILE" ;;
    *) die "category must be passed|failed|all|custom" ;;
  esac
  if [ -n "$ONLY" ]; then
    local f="$META/.only-$$.tsv"; grep -E "$ONLY" "$base" > "$f" || true; base="$f"
  fi
  if [ -n "$SHARD" ]; then
    local i="${SHARD%/*}" m="${SHARD#*/}"
    local f="$META/.shard-${i}of${m}-$$.tsv"
    awk -v i="$i" -v m="$m" 'NR % m == (i-1) % m' "$base" > "$f"
    base="$f"
  fi
  echo "$base"
}

mode_name() {  # from JIT/JDK toggles
  local j="jit" d="real"
  [ "$JIT" = "off" ] && j="nojit"
  [ "$JDK" = "synthetic" ] && d="syn"
  echo "$j-$d"
}

cmd_run() {
  parse_run_args "$@"
  compile_krun
  local listf; listf="$(list_for_category)"
  local mode; mode="$(mode_name)"
  local stamp; stamp="$(date +%Y%m%d-%H%M%S)"
  local out="$OUTROOT/${TAG:+$TAG-}${mode}-${CATEGORY}-${stamp}"
  log "RUN mode=$mode category=$CATEGORY start=$START count=$COUNT -> $out"
  run_mode "$mode" "$listf" "$START" "$COUNT" "$out"
}

cmd_hotspot() {
  parse_run_args "$@"
  compile_krun
  local listf; listf="$(list_for_category)"
  local stamp; stamp="$(date +%Y%m%d-%H%M%S)"
  local out="$OUTROOT/${TAG:+$TAG-}hotspot-${CATEGORY}-${stamp}"
  log "HOTSPOT baseline category=$CATEGORY start=$START count=$COUNT -> $out"
  run_mode "hotspot" "$listf" "$START" "$COUNT" "$out"
}

# all 4 cratonvm modes at once, each its own background process + log dir
cmd_quad() {
  parse_run_args "$@"
  compile_krun
  local listf; listf="$(list_for_category)"
  local stamp; stamp="$(date +%Y%m%d-%H%M%S)"
  local base="$OUTROOT/${TAG:+$TAG-}quad-${CATEGORY}-${stamp}"
  mkdir -p "$base"
  log "QUAD: 4 modes in parallel, category=$CATEGORY start=$START count=$COUNT -> $base"
  local m
  for m in jit-real nojit-real jit-syn nojit-syn; do
    ( run_mode "$m" "$listf" "$START" "$COUNT" "$base/$m" ) >"$base/$m.stdout.log" 2>&1 &
    log "  started $m (pid $!)"
  done
  wait
  log "QUAD complete. Summaries:"
  for m in jit-real nojit-real jit-syn nojit-syn; do
    echo "--- $m ---"; cat "$base/$m/summary.txt" 2>/dev/null || echo "(no summary)"
  done | tee "$base/QUAD-SUMMARY.txt"
}

usage() {
  cat <<EOF
run-suite.sh — Spring suite driver for CratonVM

COMMANDS
  discover                       Build the master class index (meta/all-classes.tsv)
  categorize [results.tsv]       Build meta/passed.tsv + meta/others.tsv.
                                 With no arg, runs the jit-real baseline over the
                                 whole suite first, then splits by status==OK.
  run    [opts]                  Run ONE cratonvm mode over a category slice.
  quad   [opts]                  Run ALL 4 cratonvm modes in parallel.
  hotspot[opts]                  Run the slice under HotSpot java.exe (baseline).

OPTIONS (run / quad / hotspot)
  --category passed|failed|all   class set (default all)
  --jit      on|off              JIT toggle           (run only; default on)
  --jdk      real|synthetic      JDK backend          (run only; default real)
  --start    N                   1-based start index into the category list (0=begin)
  --count    N                   number of classes (0=all)
  --batch    N                   classes per VM invocation (default $BATCH)
  --batch-to S                   per-batch timeout seconds (default $BATCH_TO)
  --one-to   S                   per-class re-run timeout seconds (default $ONE_TO)
  --tag      NAME                label prefix on the output dir

ENV
  CRATONVM_BIN=...   override cratonvm.exe path
  EXTRA_VM_ARGS=...  extra cratonvm CLI args (e.g. "-Xmx2g")
  CRATONVM_*=...     any CratonVM env knob — inherited by the VM automatically
  SPRING=...  OUTROOT=...  JDK25=...  BATCH=...  BATCH_TO=...  ONE_TO=...

OUTPUT (per mode dir under out/)
  results.tsv   class  status  found  succ  fail  skip  abort  ms  mode
  timing.tsv    class  ms  status  mode
  raw.log  failcauses.log  crashes.log  run.log  summary.txt
EOF
}

# --------------------------------------------------------------------- main ---
cmd="${1:-help}"; shift || true
case "$cmd" in
  discover)   discover ;;
  categorize) categorize "${1:-}" ;;
  run)        cmd_run "$@" ;;
  quad)       cmd_quad "$@" ;;
  hotspot)    cmd_hotspot "$@" ;;
  help|-h|--help) usage ;;
  *) usage; die "unknown command: $cmd" ;;
esac
