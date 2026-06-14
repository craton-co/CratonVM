#!/usr/bin/env bash
# Run the WildFly testsuite under CratonVM, per test class, batched with
# crash-recovery. One cratonvm.exe JVM per batch (amortises cold per-JVM JUnit
# init); if a batch JVM crashes/hangs, every class it didn't already emit a
# RESULT for is re-run individually so its true status (OK/FAIL/ABEND/TIMEOUT/
# LOADERR) is isolated. NEVER stops on failure — every class is accounted for.
#
# Boots JDK 25 (CratonVM <19 silently no-ops new Thread(runnable) -> false hangs).
# External `timeout` is the SOLE hang detector (in-VM watchdog disabled).
# Resumable: classes already present in results.tsv are skipped.
#
# Env: BATCH (default 12), BATCH_TO (batch timeout s, 900), ONE_TO (per-class
#      re-run timeout s, 180), ONLY (regex filter on module path), LIMIT,
#      MODONLY (regex on module dir).
set -u
ROOT="/c/craton/cratonvm/apps/wildfly/testsuite"
HARNESS="/c/craton/CratonVM-wildfly/wildfly-suite"
VM="${VM:-/c/craton/cratonvm/target/release/cratonvm.exe}"
JDK25="C:\\Program Files\\Eclipse Adoptium\\jdk-25.0.2.10-hotspot"
BATCH="${BATCH:-12}"; BATCH_TO="${BATCH_TO:-900}"; ONE_TO="${ONE_TO:-180}"
OUT="$HARNESS/results"; mkdir -p "$OUT"
RES="$OUT/results.tsv"; CRASH="$OUT/crashes.log"; FC="$OUT/failcauses.log"
MODLOG="$OUT/modules.log"; PROG="$OUT/progress.log"

LOCK="$OUT/.lock"
if ! mkdir "$LOCK" 2>/dev/null; then
  echo "another run-wildfly.sh is active ($(cat "$LOCK/pid" 2>/dev/null)); exiting."; exit 0
fi
echo "$$" > "$LOCK/pid"
trap 'rm -rf "$LOCK" 2>/dev/null' EXIT
touch "$RES" "$CRASH" "$FC" "$MODLOG" "$PROG"

export CRATONVM_DISABLE_DEFAULT_WATCHDOG=1

# Fixed harness classpath (KRun + junit platform/vintage/jupiter) in Windows
# (forward-slash) form, since cratonvm.exe is a native Windows program and cannot
# resolve git-bash /c/... paths.
HARNESS_W=$(cygpath -m "$HARNESS")
HCP="$HARNESS_W"
while IFS= read -r l || [ -n "$l" ]; do l="${l%$'\r'}"; [ -n "$l" ] && HCP="$HCP;$l"; done < "$HARNESS/harness-cp.txt"

record() {  # parse KRun blob -> class<TAB>status<TAB>found<TAB>succ<TAB>fail
  printf '%s\n' "$1" | awk '
    /^RESULT / { c=$2; delete v; for(i=3;i<=NF;i++){split($i,a,"=");v[a[1]]=a[2]}
                 printf "%s\t%s\t%s\t%s\t%s\n", c, v["status"], v["found"], v["succ"], v["fail"] }'
}
already() { grep -qF -- "$1"$'\t' "$RES"; }

run_one() {  # class  CP -> append result; capture crash
  local cls="$1" cp="$2" raw rc line af
  af="$OUT/.af_one.txt"; { echo "-cp"; echo "$cp"; } > "$af"
  raw=$(timeout "$ONE_TO" "$VM" --java-home "$JDK25" "@$(cygpath -m "$af")" KRun "$cls" 2>"$OUT/.err"); rc=$?
  line=$(printf '%s\n' "$raw" | grep -m1 '^RESULT ')
  printf '%s\n' "$raw" | grep -E '^FAILCAUSE|^LOADERR' >> "$FC"
  if [ $rc -eq 124 ]; then
    printf '%s\tTIMEOUT\t0\t0\t0\n' "$cls" >> "$RES"
    { echo "===== $cls [TIMEOUT rc=124] ====="; printf '%s\n' "$raw" | tail -20; echo "--err--"; tail -25 "$OUT/.err"; echo; } >> "$CRASH"
  elif [ -n "$line" ]; then
    record "$raw" >> "$RES"
    # an OK/FAIL line that ALSO crashed (rc!=0) is still a VM defect worth logging
    if [ $rc -ne 0 ] && [ $rc -ne 1 ]; then
      { echo "===== $cls [POST-RESULT rc=$rc] ====="; printf '%s\n' "$raw" | tail -8; echo "--err--"; tail -15 "$OUT/.err"; echo; } >> "$CRASH"
    fi
  else
    printf '%s\tABEND\t0\t0\t0\n' "$cls" >> "$RES"
    { echo "===== $cls [ABEND rc=$rc] ====="; printf '%s\n' "$raw" | tail -20; echo "--err--"; tail -25 "$OUT/.err"; echo; } >> "$CRASH"
  fi
}

# Discover leaf modules with compiled test classes.
mapfile -t TCDIRS < <(find "$ROOT" -type d -path '*/target/test-classes' 2>/dev/null | sort)
[ -n "${MODONLY:-}" ] && TCDIRS=($(printf '%s\n' "${TCDIRS[@]}" | grep -E "$MODONLY"))
echo "modules with test-classes: ${#TCDIRS[@]}" | tee -a "$PROG"

for TC in "${TCDIRS[@]}"; do
  MOD=$(dirname "$(dirname "$TC")")
  MCP="$HCP;$(cygpath -m "$TC");$(cygpath -m "$MOD/target/classes")"
  if [ -f "$MOD/target/cratonvm-testcp.txt" ]; then
    # build-classpath writes one line of ';'-joined Windows '\' paths -> forward-slash
    DEPS=$(tr '\\' '/' < "$MOD/target/cratonvm-testcp.txt" | tr -d '\r')
    [ -n "$DEPS" ] && MCP="$MCP;$DEPS"
  fi
  # enumerate test classes (WildFly: *TestCase predominal, also *Test); no inner classes
  mapfile -t ALL < <(cd "$TC" && find . \( -name '*TestCase.class' -o -name '*Test.class' \) ! -name '*\$*' \
      | sed 's|^\./||; s|\.class$||; s|/|.|g' | sort)
  [ -n "${ONLY:-}" ] && ALL=($(printf '%s\n' "${ALL[@]}" | grep -E "$ONLY"))
  # resume: drop already-recorded
  CLASSES=(); for c in "${ALL[@]}"; do already "$c" || CLASSES+=("$c"); done
  [ -n "${LIMIT:-}" ] && CLASSES=("${CLASSES[@]:0:$LIMIT}")
  mtotal=${#CLASSES[@]}
  echo "MODULE $MOD  classes=${#ALL[@]} todo=$mtotal" | tee -a "$MODLOG" | tee -a "$PROG"
  [ "$mtotal" -eq 0 ] && continue

  for ((b=0; b<mtotal; b+=BATCH)); do
    batch=("${CLASSES[@]:b:BATCH}")
    { echo "-cp"; echo "$MCP"; } > "$OUT/.af_batch.txt"
    raw=$(timeout "$BATCH_TO" "$VM" --java-home "$JDK25" "@$(cygpath -m "$OUT/.af_batch.txt")" KRun "${batch[@]}" 2>"$OUT/.err"); rc=$?
    printf '%s\n' "$raw" | grep -E '^FAILCAUSE|^LOADERR' >> "$FC"
    mapfile -t got < <(printf '%s\n' "$raw" | sed -n 's/^RESULT \([^ ]*\) .*/\1/p')
    record "$raw" >> "$RES"
    if [ $rc -ne 0 ]; then
      { echo "===== BATCH crash in $MOD [rc=$rc], last BEGIN below ====="; printf '%s\n' "$raw" | tail -12; echo "--err--"; tail -20 "$OUT/.err"; echo; } >> "$CRASH"
    fi
    for cls in "${batch[@]}"; do
      printf '%s\n' "${got[@]}" | grep -qxF "$cls" && continue
      run_one "$cls" "$MCP"
    done
    printf '[%s] %d/%d batch rc=%d (%d direct)\n' "$(basename "$MOD")" "$((b+${#batch[@]}))" "$mtotal" "$rc" "${#got[@]}" | tee -a "$PROG"
  done
done

echo "=================== AGGREGATE ===================" | tee -a "$PROG"
awk -F'\t' '{c[$2]++; f+=$3; s+=$4; x+=$5} END{
  print "classes by status:"; for (k in c) printf "  %-9s %d\n", k, c[k];
  print "test-level: found="f" succeeded="s" failed="x}' "$RES" | tee "$OUT/summary.txt" | tee -a "$PROG"
echo "results=$RES crashes=$CRASH failcauses=$FC" | tee -a "$PROG"
