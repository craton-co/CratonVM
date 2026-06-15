#!/usr/bin/env bash
# Phase 1: run the COMPLETE Spring test suite under CratonVM, per test class,
# batched with crash-recovery. One cratonvm.exe per batch (amortises cold JUnit
# init); if a batch JVM crashes/hangs, every class it did NOT already emit a
# RESULT for is re-run individually to isolate its true status
# (OK/FAIL/EMPTY/CRASH/ABEND/TIMEOUT/LOADERR). NEVER stops on failure — every
# class is accounted for, so we get exact total numbers.
#
# Boots HotSpot JDK 25 (CratonVM <19 silently no-ops new Thread(runnable)).
# External `timeout` is the sole hang detector. Resumable: classes already in
# results.tsv are skipped.
#
# Env: BATCH (default 12), BATCH_TO (batch timeout s, 600), ONE_TO (per-class
#      re-run timeout s, 180), MODONLY (regex on module dir), ONLY (regex on
#      class), LIMIT (cap classes per module), VM (override binary).
set -u
export MSYS2_ARG_CONV_EXCL='*'; export MSYS_NO_PATHCONV=1
SPRING="/c/craton/cratonvm/apps/spring-framework"
HARNESS="/c/craton/CratonVM-spring/spring-suite"
VM="${VM:-/c/craton/CratonVM-spring/target/release/cratonvm.exe}"
[ -x "$VM" ] || VM="/c/craton/CratonVM/target/release/cratonvm.exe"
JDK25="C:\\Program Files\\Eclipse Adoptium\\jdk-25.0.2.10-hotspot"
BATCH="${BATCH:-12}"; BATCH_TO="${BATCH_TO:-600}"; ONE_TO="${ONE_TO:-180}"
OUT="${OUT:-$HARNESS/results-cv}"; mkdir -p "$OUT"
RES="$OUT/results.tsv"; CRASH="$OUT/crashes.log"; FC="$OUT/failcauses.log"
PROG="$OUT/progress.log"
touch "$RES" "$CRASH" "$FC" "$PROG"
export CRATONVM_DISABLE_DEFAULT_WATCHDOG=1

KRUN_W=$(cygpath -m "$HARNESS")

record() {  # parse KRun blob -> class<TAB>status<TAB>found<TAB>succ<TAB>fail<TAB>skip<TAB>abort
  printf '%s\n' "$1" | awk '
    /^RESULT / { c=$2; delete v; for(i=3;i<=NF;i++){split($i,a,"=");v[a[1]]=a[2]}
       printf "%s\t%s\t%s\t%s\t%s\t%s\t%s\n", c, v["status"], v["found"]+0, v["succ"]+0, v["fail"]+0, v["skip"]+0, v["abort"]+0 }'
}
already() { grep -qF -- "$1"$'\t' "$RES"; }

classify_crash() {  # $1 raw stdout, $2 errfile -> echo CRASH or ABEND
  if grep -qaiE "EXCEPTION_ACCESS_VIOLATION|SIGSEGV|STATUS_|fatal runtime error|panicked at|stack (overflow|smashing)|illegal instruction|assertion failed|not yet implemented|unreachable|index out of bounds|RUST_BACKTRACE|thread '.*' panicked" "$2"; then
    echo CRASH; else echo ABEND; fi
}

run_one() {  # class  argfile -> append result; capture crash detail
  local cls="$1" af="$2" raw rc line
  raw=$(timeout "$ONE_TO" "$VM" --java-home "$JDK25" --stack-dump-on-timeout 0 "@$af" KRun "$cls" 2>"$OUT/.err"); rc=$?
  line=$(printf '%s\n' "$raw" | grep -m1 '^RESULT ')
  printf '%s\n' "$raw" | grep -E '^FAILCAUSE|^LOADERR' >> "$FC"
  if [ $rc -eq 124 ]; then
    printf '%s\tTIMEOUT\t0\t0\t0\t0\t0\n' "$cls" >> "$RES"
    { echo "===== $cls [TIMEOUT rc=124] ====="; printf '%s\n' "$raw" | tail -15; echo "--err--"; tail -25 "$OUT/.err"; echo; } >> "$CRASH"
  elif [ -n "$line" ]; then
    record "$raw" >> "$RES"
    if [ $rc -ne 0 ] && [ $rc -ne 1 ]; then
      local kind=$(classify_crash "$raw" "$OUT/.err")
      { echo "===== $cls [POST-RESULT rc=$rc $kind] ====="; printf '%s\n' "$raw" | tail -6; echo "--err--"; tail -20 "$OUT/.err"; echo; } >> "$CRASH"
    fi
  else
    local kind=$(classify_crash "$raw" "$OUT/.err")
    printf '%s\t%s\t0\t0\t0\t0\t0\n' "$cls" "$kind" >> "$RES"
    { echo "===== $cls [$kind rc=$rc] ====="; printf '%s\n' "$raw" | tail -15; echo "--err--"; tail -25 "$OUT/.err"; echo; } >> "$CRASH"
  fi
}

# Discover modules with compiled test classes (java/kotlin/groovy).
mapfile -t TESTDIRS < <(find "$SPRING" -type d -path '*/build/classes/*/test' 2>/dev/null | sort -u)
# Map to module roots
declare -A MODS
for td in "${TESTDIRS[@]}"; do
  mod="${td%/build/classes/*}"
  MODS["$mod"]=1
done
# Priority order: fast/high-signal core unit-test modules first; Kotlin-heavy /
# integration / docs modules last (they grind on kotlin-reflect and dominate
# wall-clock). Unlisted modules get rank 50.
PRIO="spring-core spring-beans spring-expression spring-aop spring-context spring-tx spring-jdbc spring-messaging spring-web spring-jms spring-orm spring-oxm spring-r2dbc spring-context-support spring-aspects spring-webflux spring-websocket spring-webmvc spring-core-test spring-context-indexer spring-instrument spring-test framework-docs integration-tests"
rank() { local b="$1" i=1 p; for p in $PRIO; do [ "$p" = "$b" ] && { echo $i; return; }; i=$((i+1)); done; echo 50; }
tmp=()
for m in "${!MODS[@]}"; do tmp+=("$(printf '%03d' "$(rank "$(basename "$m")")") $m"); done
MODLIST=($(printf '%s\n' "${tmp[@]}" | sort | sed 's/^[0-9]* //'))
[ -n "${MODONLY:-}" ] && MODLIST=($(printf '%s\n' "${MODLIST[@]}" | grep -E "$MODONLY"))
echo "modules with test-classes: ${#MODLIST[@]} (order: $(for m in "${MODLIST[@]}"; do basename "$m"; done | tr '\n' ' '))" | tee -a "$PROG"

for MOD in "${MODLIST[@]}"; do
  MODNAME=$(basename "$MOD")
  CPF="$MOD/build/cratonvm-testcp.txt"
  if [ ! -f "$CPF" ]; then echo "SKIP $MODNAME (no cratonvm-testcp.txt)" | tee -a "$PROG"; continue; fi
  # Harness dir first (KRun.class), then the full dumped runtime classpath.
  MCP="$KRUN_W;$(tr -d '\r' < "$CPF")"
  AF="$OUT/.af_$MODNAME.txt"; { echo "-cp"; echo "$MCP"; } > "$AF"
  AFM=$(cygpath -m "$AF")
  # enumerate concrete-looking test classes across all test output dirs
  mapfile -t ALL < <(for d in "$MOD"/build/classes/*/test; do
      [ -d "$d" ] && (cd "$d" && find . \( -name '*Tests.class' -o -name '*Test.class' \) ! -name '*$*' \
        | sed 's|^\./||; s|\.class$||; s|/|.|g'); done | sort -u)
  [ -n "${ONLY:-}" ] && ALL=($(printf '%s\n' "${ALL[@]}" | grep -E "$ONLY"))
  # Class-level sharding for parallel workers: keep only this shard's classes.
  if [ -n "${SHARD_N:-}" ]; then
    ALL=($(printf '%s\n' "${ALL[@]}" | awk -v n="$SHARD_N" -v id="${SHARD_ID:-0}" '((NR-1)%n)==id'))
  fi
  CLASSES=(); for c in "${ALL[@]}"; do already "$c" || CLASSES+=("$c"); done
  [ -n "${LIMIT:-}" ] && CLASSES=("${CLASSES[@]:0:$LIMIT}")
  mtotal=${#CLASSES[@]}
  echo "MODULE $MODNAME  classes=${#ALL[@]} todo=$mtotal" | tee -a "$PROG"
  [ "$mtotal" -eq 0 ] && continue

  for ((b=0; b<mtotal; b+=BATCH)); do
    batch=("${CLASSES[@]:b:BATCH}")
    raw=$(timeout "$BATCH_TO" "$VM" --java-home "$JDK25" --stack-dump-on-timeout 0 "@$AFM" KRun "${batch[@]}" 2>"$OUT/.err"); rc=$?
    printf '%s\n' "$raw" | grep -E '^FAILCAUSE|^LOADERR' >> "$FC"
    mapfile -t got < <(printf '%s\n' "$raw" | sed -n 's/^RESULT \([^ ]*\) .*/\1/p')
    record "$raw" >> "$RES"
    if [ $rc -ne 0 ]; then
      local_kind=$(classify_crash "$raw" "$OUT/.err")
      { echo "===== BATCH $MODNAME [rc=$rc $local_kind], tail below ====="; printf '%s\n' "$raw" | tail -10; echo "--err--"; tail -20 "$OUT/.err"; echo; } >> "$CRASH"
    fi
    for cls in "${batch[@]}"; do
      printf '%s\n' "${got[@]}" | grep -qxF "$cls" && continue
      run_one "$cls" "$AFM"
    done
    printf '[%s] %d/%d (batch rc=%d, %d direct)\n' "$MODNAME" "$((b+${#batch[@]}))" "$mtotal" "$rc" "${#got[@]}" | tee -a "$PROG"
  done
done

echo "=================== CratonVM AGGREGATE ===================" | tee -a "$PROG"
awk -F'\t' '{c[$2]++; f+=$3; s+=$4; x+=$5} END{
  print "classes by status:"; for (k in c) printf "  %-9s %d\n", k, c[k];
  print "test-level: found="f" succeeded="s" failed="x}' "$RES" | tee "$OUT/summary.txt" | tee -a "$PROG"
echo "results=$RES crashes=$CRASH failcauses=$FC" | tee -a "$PROG"
