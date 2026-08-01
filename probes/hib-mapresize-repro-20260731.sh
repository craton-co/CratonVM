#!/usr/bin/env bash
# HIB-MAPRESIZE-STALE.1 reproducer.
#
# Runs DefaultCatalogAndSchemaTest exactly as the known-issue doc specifies:
# --nojit, --Xmx 1500m, real JDK, batch=1. Baseline takes ~17-40 min and ends
# in rc=139 (SIGSEGV) with no @@RESULT line.
#
#   CV=<cratonvm.exe>   binary under test (required)
#   TAG=<name>          output file tag
#   Extra CRATONVM_* env vars are inherited by the VM.
set -uo pipefail
export MSYS2_ARG_CONV_EXCL='*'
export MSYS_NO_PATHCONV=1

FIXTURE="C:/craton/CratonVM/apps/hib-suite-runner"
CV="${CV:?set CV to the cratonvm.exe under test}"
TAG="${TAG:-baseline}"
XMX="${XMX:-1500m}"
CLASS="${CLASS:-org.hibernate.orm.test.boot.database.qualfiedTableNaming.DefaultCatalogAndSchemaTest}"
OUT="${OUT:-$FIXTURE/runs/mapstale-$TAG.log}"

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
JDK="${JDK:-$(detect_jdk)}"
[ -n "$JDK" ] || { echo "no JDK found" >&2; exit 2; }

mkdir -p "$(dirname "$OUT")"
echo "=== $TAG  CV=$CV  JDK=$JDK  XMX=$XMX ===" | tee "$OUT"
echo "=== env: $(env | grep '^CRATONVM_' | sort | tr '\n' ' ') ===" | tee -a "$OUT"
start=$(date +%s)
(
  cd "$FIXTURE" || exit 2
  "$CV" --java-home "$JDK" --Xmx "$XMX" --nojit "@$FIXTURE/common.args" \
        -Dcraton.batch=1 CratonRunner "$CLASS"
) >>"$OUT" 2>&1
rc=$?
end=$(date +%s)
echo "=== rc=$rc elapsed=$((end-start))s ===" | tee -a "$OUT"
grep -c 'out-of-bounds field write dropped' "$OUT" | sed 's/^/drops=/' | tee -a "$OUT"
grep -m1 '@@RESULT' "$OUT" | tee -a "$OUT"
exit $rc
