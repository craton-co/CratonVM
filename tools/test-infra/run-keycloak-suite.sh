#!/usr/bin/env bash
# Run keycloak core + crypto/default JUnit4 suites under CratonVM and HotSpot,
# one VM per concrete test class, and emit a side-by-side timing/correctness
# comparison. See memory: reference_keycloak_test_harness.
set +e
export MSYS2_ARG_CONV_EXCL='*'
export MSYS_NO_PATHCONV=1

ROOT="${ROOT:-C:/craton/CratonVM}"
KC="$ROOT/apps/keycloak"
CV="${CV:-$ROOT/target/release/cratonvm.exe}"
JDK="${JDK:-C:/Program Files/Java/jdk-25}"
HS="$JDK/bin/java.exe"
JAVAP="$JDK/bin/javap.exe"
TIMEOUT="${TIMEOUT:-240}"
TS=$(date +%Y%m%d-%H%M%S)
LOGDIR="$ROOT/test-infra/suite-results/keycloak-$TS"
OUT="$LOGDIR/results.tsv"
MD="$LOGDIR/keycloak-comparison.md"

mkdir -p "$LOGDIR"
[ -x "$CV" ] || { echo "ERROR: missing $CV"; exit 3; }
printf "module\tclass\tvm\trc\tstate\twall_s\ttests\tfail\tnote\n" > "$OUT"
taskkill //F //IM cratonvm.exe 2>/dev/null || true

CORE_CP="$KC/core/target/classes;$KC/core/target/test-classes;$(cat "$KC/core/cratonvm-core-cp.txt")"
CRYPTO_CP="$KC/crypto/default/target/classes;$KC/crypto/default/target/test-classes;$(cat "$KC/crypto/default/cratonvm-crypto-cp.txt")"

TESTS=0; FAIL=0; STATE=PASS
parse_junit() { # $1 log
  TESTS=$(grep -aoE "OK \([0-9]+ test" "$1" | grep -oE "[0-9]+" | tail -1)
  [ -z "$TESTS" ] && TESTS=$(grep -aoE "Tests run: [0-9]+" "$1" | grep -oE "[0-9]+" | tail -1)
  FAIL=$(grep -aoE "Failures: [0-9]+" "$1" | grep -oE "[0-9]+" | tail -1)
  [ -z "$TESTS" ] && TESTS=0
  [ -z "$FAIL" ] && FAIL=0
}
classify() { # $1 log, $2 rc
  STATE=PASS
  if [ "$2" = 124 ]; then STATE=TIMEOUT; return; fi
  if grep -qaiE "EXCEPTION_ACCESS_VIOLATION|SIGSEGV|fatal runtime error|panicked at|stack smashing|illegal instruction" "$1"; then STATE=CRASH; return; fi
  if grep -qa "FAILURES!!!" "$1"; then STATE=FAIL; return; fi
  if grep -qa "^OK (" "$1"; then STATE=PASS; return; fi
  [ "$2" != 0 ] && STATE=FAIL
}
short_note() { # $1 log
  grep -aE "^[0-9]+\)|Exception|Error|Caused by|not implemented" "$1" \
    | grep -avE "^\s*at |OK \(" | head -1 | sed 's/\x1b\[[0-9;]*m//g' | head -c 110
}

is_abstract() { # $1 cp, $2 fqcn
  # javap's first line is `Compiled from "..."`; the class declaration is on a
  # later line (e.g. `public abstract class org.keycloak.RSAVerifierTest {`).
  "$JAVAP" -cp "$1" -p "$2" 2>/dev/null | grep -qE "(^| )abstract (class|interface) "
}

run_class() { # $1 module, $2 cp, $3 fqcn
  local module="$1" cp="$2" fqcn="$3"
  if is_abstract "$cp" "$fqcn"; then
    printf "  %-66s ABSTRACT (skip)\n" "$fqcn"
    printf "%s\t%s\t-\t-\tABSTRACT\t-\t-\t-\t-\n" "$module" "$fqcn" >> "$OUT"
    return
  fi
  # ---- CratonVM ----
  local cvlog="$LOGDIR/$fqcn.cv.log"
  local t0 t1 rc wall
  t0=$(date +%s%N)
  timeout "$TIMEOUT" "$CV" --java-home "$JDK" --stack-dump-on-timeout 0 \
    -cp "$cp" org.junit.runner.JUnitCore "$fqcn" </dev/null >"$cvlog" 2>&1
  rc=$?; t1=$(date +%s%N)
  wall=$(awk -v ms=$(((t1-t0)/1000000)) 'BEGIN{printf "%.1f", ms/1000}')
  parse_junit "$cvlog"; classify "$cvlog" "$rc"
  local cv_state=$STATE cv_tests=$TESTS cv_fail=$FAIL cv_wall=$wall
  local note; note=$(short_note "$cvlog")
  printf "  %-66s CV rc=%s %-7s %5ss  %s/%s  %s\n" "$fqcn" "$rc" "$cv_state" "$cv_wall" "$cv_fail" "$cv_tests" "$note"
  printf "%s\t%s\tcratonvm\t%s\t%s\t%s\t%s\t%s\t%s\n" "$module" "$fqcn" "$rc" "$cv_state" "$cv_wall" "$cv_tests" "$cv_fail" "$note" >> "$OUT"
  # ---- HotSpot (always, for timing) ----
  local hslog="$LOGDIR/$fqcn.hs.log"
  t0=$(date +%s%N)
  timeout "$TIMEOUT" "$HS" -cp "$cp" org.junit.runner.JUnitCore "$fqcn" </dev/null >"$hslog" 2>&1
  rc=$?; t1=$(date +%s%N)
  wall=$(awk -v ms=$(((t1-t0)/1000000)) 'BEGIN{printf "%.1f", ms/1000}')
  parse_junit "$hslog"; classify "$hslog" "$rc"
  printf "  %-66s HS rc=%s %-7s %5ss  %s/%s\n" "" "$rc" "$STATE" "$wall" "$FAIL" "$TESTS"
  printf "%s\t%s\thotspot\t%s\t%s\t%s\t%s\t%s\t-\n" "$module" "$fqcn" "$rc" "$STATE" "$wall" "$TESTS" "$FAIL" >> "$OUT"
}

echo "###### keycloak core ######"
for fqcn in $(find "$KC/core/target/test-classes" -name '*Test.class' \
    | sed "s#$KC/core/target/test-classes/##; s#/#.#g; s#.class\$##" | sort); do
  run_class core "$CORE_CP" "$fqcn"
done

echo "###### keycloak crypto/default ######"
for fqcn in $(find "$KC/crypto/default/target/test-classes" -name '*Test.class' \
    | sed "s#$KC/crypto/default/target/test-classes/##; s#/#.#g; s#.class\$##" | sort); do
  run_class crypto "$CRYPTO_CP" "$fqcn"
done

# ---------- report ----------
{
  echo "# Keycloak suite — CratonVM vs HotSpot"
  echo
  echo "**Date:** $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "**CratonVM:** \`$CV\`  (HEAD $(cd "$ROOT" && git rev-parse --short HEAD))"
  echo "**HotSpot:** \`$HS\` ($("$HS" -version 2>&1 | head -1 | sed 's/"//g'))"
  echo "**Per-class timeout:** ${TIMEOUT}s   **Logs:** \`$LOGDIR/\`"
  echo
  echo "| Module | Test class | CratonVM | tests | HotSpot | CV time | HS time | Slowdown |"
  echo "|--------|-----------|----------|-------|---------|---------|---------|----------|"
  awk -F'\t' 'NR>1{
      k=$1 SUBSEP $2; mod[k]=$1; cls[k]=$2;
      if($3=="cratonvm"){cvst[k]=$5;cvwall[k]=$6;cvt[k]=$7;cvf[k]=$8}
      else if($3=="hotspot"){hsst[k]=$5;hswall[k]=$6;hst[k]=$7}
      else {cvst[k]="ABSTRACT"}
      seen[k]=1 }
    END{ for(k in seen){
        if(cvst[k]=="ABSTRACT") continue;
        sd="-";
        if(hswall[k]+0>0 && cvwall[k]+0>0) sd=sprintf("%.1fx", cvwall[k]/hswall[k]);
        tj=(cvf[k]+0>0)? sprintf("%s/%s (%s fail)",cvt[k]-cvf[k],cvt[k],cvf[k]) : cvt[k];
        printf "| %s | %s | %s | %s | %s | %ss | %ss | %s |\n",
          mod[k], cls[k], cvst[k], tj, (hsst[k]==""?"—":hsst[k]), cvwall[k], (hswall[k]==""?"—":hswall[k]), sd
      } }' "$OUT" | sort
  echo
  echo "## Summary"
  awk -F'\t' 'NR>1 && $3=="cratonvm"{
      tot++; st[$5]++; cvsum+=$6; ttot+=$7; tfail+=$8 }
    NR>1 && $3=="hotspot"{hssum+=$6}
    END{
      printf "\n- Concrete classes run: **%d**\n", tot;
      for(s in st) printf "  - %s: %d\n", s, st[s];
      printf "- Tests executed (CratonVM): **%d**, failures: **%d**\n", ttot, tfail;
      printf "- Total wall: CratonVM **%.1fs** vs HotSpot **%.1fs** (%.1fx)\n", cvsum, hssum, (hssum>0?cvsum/hssum:0);
    }' "$OUT"
} > "$MD"

echo
echo "==== wrote $MD ===="
cat "$MD"
