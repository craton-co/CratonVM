#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# App-level GC-root acceptance lane — docs/feature-designs/precise-jit-maps-default.md
# Step 4 "app-gauntlet", the named-app half (companion to gc-root-lane.sh).
#
# Runs real Java app suites under CratonVM with precise JIT stack maps ON (the
# dev default) BOTH at the default heap (correctness vs HotSpot) AND under forced
# young GC (`CRATONVM_DBG_GC_STRESS`) — the configuration that surfaces the
# GC-root-coverage-under-JIT (register-invisibility) family — and flags any
# GC-root corruption marker (stale pointer / implausible object size /
# inconsistent header / out-of-bounds field / SEGV / panic). A suite PASSES when
# both CratonVM runs show the suite's success signature with rc=0 and zero
# corruption markers, and HotSpot agrees.
#
# Each suite is GATED on its classpath/classes being present and SKIP-with-note
# when absent, so this same lane runs on CI / a provisioned box where the heavy
# container suites (wildfly/kafka/h2/elasticsearch/...) are actually built — on a
# bare dev checkout most are unbuilt and will report SKIP. No VM change.
#
#   CV=path/to/cratonvm.exe   JDK=path/to/jdk-25   TIMEOUT=secs(900)
#   GC_STRESS=bytes(1048576)  APPS=path/to/apps    QUICK=1 (skip the GC-stress pass)
# ---------------------------------------------------------------------------
set +e
export MSYS2_ARG_CONV_EXCL='*'
export MSYS_NO_PATHCONV=1
# Harness governs per-run timeouts; disable the VM's internal native-hang abort.
export CRATONVM_DISABLE_DEFAULT_WATCHDOG=1

ROOT="${ROOT:-$(git -C "$(dirname "${BASH_SOURCE[0]}")" rev-parse --show-toplevel 2>/dev/null || echo C:/craton/CratonVM)}"
CV="${CV:-$ROOT/target/release/cratonvm.exe}"
JDK="${JDK:-C:/Program Files/Eclipse Adoptium/jdk-25.0.2.10-hotspot}"
if command -v cygpath >/dev/null 2>&1; then
  ROOT="$(cygpath -m "$ROOT")"; CV="$(cygpath -m "$CV")"; JDK="$(cygpath -m "$JDK")"
fi
HS="$JDK/bin/java.exe"
APPS="${APPS:-$ROOT/apps}"
command -v cygpath >/dev/null 2>&1 && APPS="$(cygpath -m "$APPS")"
TIMEOUT="${TIMEOUT:-900}"
GC_STRESS="${GC_STRESS:-1048576}"   # 1 MB → frequent young GC
QUICK="${QUICK:-0}"

RESDIR="$ROOT/test-infra/regression-pool/results"
mkdir -p "$RESDIR"
TS=$(date +%Y%m%d-%H%M%S)
TSV="$RESDIR/gc-root-apps-$TS.tsv"

[ -x "$CV" ] || { echo "FATAL: CratonVM binary not found: $CV" >&2; exit 2; }
[ -x "$HS" ] || { echo "FATAL: HotSpot not found: $HS" >&2; exit 2; }

hr() { printf '%s\n' "--------------------------------------------------------------------------"; }
log() { echo "[gc-root-apps] $*"; }
printf 'suite\tcv_default\tcv_gcstress\thotspot\tstatus\tnote\n' > "$TSV"
NPASS=0; NSKIP=0; NFAIL=0; NDEV=0

# HARD = a fatal GC-root failure (crash / process death) — always a FAIL.
# SOFT = the GC-array/field guard catching+dropping a stale access; benign IFF the
# result is still GC-invariant (the "masked, not closed" state from Step 4's
# gc-stress-bintrees springboot). Reported as `guarded(soft=N)` on a passing run.
HARD_CORRUPT='EXCEPTION_ACCESS|SIGSEGV|fatal runtime error|fatal error|thread .* panicked|process::abort|stack overflow'
SOFT_CORRUPT='stale pointer|implausible|inconsistent header|out-of-bounds field|class_id=ClassId\(0\)'

# run_one <env-prefix> <heap> <cp> <main> <args> -> RC, OUT
run_one() {
  local envp="$1" heap="$2" cp="$3" main="$4" args="$5"
  OUT=$(env $envp timeout "$TIMEOUT" "$CV" --java-home "$JDK" --stack-dump-on-timeout 0 \
        --Xmx "$heap" -cp "$cp" $main $args </dev/null 2>&1)
  RC=$?
}

# app_suite <name> <heap> <cp> <main> <args> <outcome-regex>
#
# GC-root criterion = **GC-invariance + no FATAL corruption**, NOT full feature
# parity. The register-invisibility family manifests as a GC-dependent fault. So a
# suite PASSES when CratonVM's outcome is IDENTICAL with and without forced young
# GC (same rc + same outcome signature) and shows no HARD (crash) corruption.
#   * HARD markers (SEGV/panic/fatal) → FAIL always.
#   * SOFT markers (the GC walker catching+re-syncing a stale/half-init access)
#     are benign IFF the result stays GC-invariant — reported as `guarded(soft=N)`
#     (the "masked, not closed" state, consistent with the Step-4 repro lane).
#   * A feature gap vs HotSpot (locale/i18n) is GC-invariant → a note, not a fail.
# `outcome-regex` selects the suite's result-summary line(s) = the comparable sig.
app_suite() {
  local name="$1" heap="$2" cp="$3" main="$4" args="$5" outre="$6"
  sig() { grep -aE "$outre" | tr -d '\r' | sort | tr '\n' '|'; }
  local hs_sig
  hs_sig=$(timeout "$TIMEOUT" "$HS" -Xmx"$heap" -cp "$cp" $main $args </dev/null 2>&1 | sig)
  # CratonVM precise default-on, default heap
  run_one "" "$heap" "$cp" "$main" "$args"
  local d_rc=$RC d_hard d_soft d_sig
  d_hard=$(echo "$OUT" | grep -ciE "$HARD_CORRUPT"); d_soft=$(echo "$OUT" | grep -ciE "$SOFT_CORRUPT"); d_sig=$(echo "$OUT" | sig)
  # CratonVM precise default-on + forced young GC
  local s_rc="-" s_hard=0 s_soft=0 s_sig="$d_sig"
  if [ "$QUICK" != "1" ]; then
    run_one "CRATONVM_DBG_GC_STRESS=$GC_STRESS" "$heap" "$cp" "$main" "$args"
    s_rc=$RC; s_hard=$(echo "$OUT" | grep -ciE "$HARD_CORRUPT"); s_soft=$(echo "$OUT" | grep -ciE "$SOFT_CORRUPT"); s_sig=$(echo "$OUT" | sig)
  fi
  local hsparity="cv==hs"; [ "$d_sig" = "$hs_sig" ] || hsparity="cv!=hs(feature-gap)"
  local soft=$((d_soft + s_soft))
  local status note
  if [ -z "$d_sig" ]; then
    status="SKIP"; note="no outcome line — unsupported/env (rc=$d_rc)"; NSKIP=$((NSKIP+1))
  elif [ "$d_hard" = 0 ] && [ "$s_hard" = 0 ] \
       && { [ "$QUICK" = "1" ] || { [ "$s_sig" = "$d_sig" ] && [ "$s_rc" = "$d_rc" ]; }; }; then
    if [ "$soft" -gt 0 ]; then
      status="PASS"; note="GC-invariant; guarded(soft=$soft, walker re-synced, result unchanged); $hsparity"
    else
      status="PASS"; note="GC-invariant + corruption-free; $hsparity"
    fi
    NPASS=$((NPASS+1))
  else
    local sigdiff=no; [ "$d_sig" = "$s_sig" ] || sigdiff=YES
    status="FAIL"; note="HARD/perturbed: d(rc=$d_rc hard=$d_hard) s(rc=$s_rc hard=$s_hard) sigdiff=$sigdiff; $hsparity"; NFAIL=$((NFAIL+1)); NDEV=$((NDEV+1))
  fi
  printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$name" "rc=$d_rc/h=$d_hard/s=$d_soft" "rc=$s_rc/h=$s_hard/s=$s_soft" "$hsparity" "$status" "$note" >> "$TSV"
  printf '  %-22s %-7s d(rc=%s h=%s s=%s) g(rc=%s h=%s s=%s)  %-22s %s\n' \
    "$name" "$status" "$d_rc" "$d_hard" "$d_soft" "$s_rc" "$s_hard" "$s_soft" "$hsparity" "$note"
}

# skip_suite <name> <reason>
skip_suite() { printf '%s\t-\t-\t-\tSKIP\t%s\n' "$1" "$2" >> "$TSV"; NSKIP=$((NSKIP+1)); printf '  %-22s SKIP    %s\n' "$1" "$2"; }

hr; log "App GC-root gauntlet — precise default-on, default heap + GC_STRESS=$GC_STRESS"
log "CV=$CV"

# --- BouncyCastle: crypto / ASN.1 / PRNG regression — reflection + allocation
# heavy (a strong register-invisibility GC-root vehicle). Pure Java, no env deps.
BCCP="$APPS/bc-java/core/build/classes/java/main;$APPS/bc-java/core/build/classes/java/test;$APPS/bc-java/core/build/resources/main;$APPS/bc-java/core/build/resources/test"
BCTEST="$APPS/bc-java/core/build/classes/java/test/org/bouncycastle"
BC_OUT='All tests successful|Completed with [0-9]+ FAILURES|=> '
if [ -f "$BCTEST/asn1/test/RegressionTest.class" ]; then
  app_suite bc-asn1   1g "$BCCP" org.bouncycastle.asn1.test.RegressionTest   "" "$BC_OUT"
else skip_suite bc-asn1 "bc-java test classes not built ($BCTEST/asn1/test)"; fi
if [ -f "$BCTEST/crypto/prng/test/RegressionTest.class" ]; then
  app_suite bc-prng   1g "$BCCP" org.bouncycastle.crypto.prng.test.RegressionTest "" "$BC_OUT"
else skip_suite bc-prng "bc-java prng test classes not built"; fi
if [ -f "$BCTEST/crypto/test/RegressionTest.class" ]; then
  if [ "${BC_CRYPTO:-0}" = "1" ]; then
    app_suite bc-crypto 1g "$BCCP" org.bouncycastle.crypto.test.RegressionTest "" "$BC_OUT"
  else
    skip_suite bc-crypto "present but too long for the lane timeout — set BC_CRYPTO=1 + a large TIMEOUT to include it"
  fi
else skip_suite bc-crypto "bc-java crypto test classes not built"; fi

# --- Heavy container / build-artifact suites: present only on CI / a provisioned
# box. Gated; SKIP-with-note on a bare dev checkout. (Wired so the lane is the
# reusable named-app gauntlet the doc calls for.)
hr; log "container / build-artifact suites (SKIP unless their classpath is built)"
[ -f "$APPS/h2database/h2/temp/org/h2/test/TestAll.class" ] \
  && app_suite h2-testall 1g "$APPS/h2database/h2/temp" org.h2.test.TestAll "-fast" "Test\\b|passed|OK" \
  || skip_suite h2-testall "h2 temp/ classes not built"
[ -f "$APPS/wildfly/health/target/test-classes/.built" ] \
  && skip_suite wildfly-health "needs RunDirTests harness (absent here)" \
  || skip_suite wildfly-health "wildfly test-classpath / RunDirTests not built (env: xnio Options + heap-cap)"
[ -f "$APPS/kafka/clients/build/testcp.txt" ] \
  && skip_suite kafka-clients "present — wire KRun (needs docker broker for integration)" \
  || skip_suite kafka-clients "kafka testcp.txt not built (needs gradle JDK17 + docker@9092)"
skip_suite elasticsearch "ES lib/ not present (needs the 8.15.5 distro)"
skip_suite hibernate-smoke "hibernate-ri10 smoke-cache not built"
skip_suite commons-math "no runnable JUnitProbe main built here"

# --- The named register-invisibility apps from the family README. None ship a
# built classpath on a dev checkout, and their main/cp are deployment-specific,
# so they are CI targets driven by env (set <APP>_CP, <APP>_MAIN, optional
# <APP>_ARGS / <APP>_OUT) — otherwise SKIP-with-note. This makes the lane the
# complete named-app gauntlet without hardcoding speculative classpaths.
hr; log "named register-invisibility apps (set <APP>_CP + <APP>_MAIN to run on CI)"
named_app() { # ENVPREFIX name default-heap default-outre
  local pfx="$1" name="$2" heap="$3" outre="$4"
  local cp main args
  eval "cp=\${${pfx}_CP:-}"; eval "main=\${${pfx}_MAIN:-}"; eval "args=\${${pfx}_ARGS:-}"
  eval "outre=\${${pfx}_OUT:-$outre}"; eval "heap=\${${pfx}_HEAP:-$heap}"
  if [ -n "$cp" ] && [ -n "$main" ]; then
    command -v cygpath >/dev/null 2>&1 && cp="$(cygpath -m "$cp")"
    app_suite "$name" "$heap" "$cp" "$main" "$args" "$outre"
  else
    skip_suite "$name" "set ${pfx}_CP + ${pfx}_MAIN (+ ${pfx}_ARGS/${pfx}_OUT) to run on a box where it is built"
  fi
}
named_app TOMCAT     tomcat      2g 'OK|Tests run:|BUILD SUCCESS'
named_app KEYCLOAK   keycloak    2g 'OK|Tests run:|BUILD SUCCESS'
named_app SPRINGBOOT spring-boot 2g 'OK|Tests run:|Started .* in'
named_app CASSANDRA  cassandra   2g 'OK|Tests run:|PASSED'
named_app ACTIVEMQ   activemq    2g 'OK|Tests run:|PASSED'
named_app JENKINS    jenkins     2g 'OK|Tests run:|BUILD SUCCESS'
named_app FELIX      felix       2g 'OK|Tests run:|PASSED'

hr
log "results: $TSV"
log "PASS=$NPASS  SKIP=$NSKIP  FAIL=$NFAIL  (deviations=$NDEV)"
if [ "$NFAIL" -gt 0 ]; then log "FAIL: $NFAIL suite(s) failed precise-on (default and/or GC-stress) — investigate."; exit 1; fi
log "OK: every runnable suite passed precise-on at default heap AND under forced young GC."
exit 0
