#!/usr/bin/env bash
# hibfix-20260822 isolated repro driver.
#   usage: hibfix-run.sh <tag> <binary|HOTSPOT> <db> [extra args...] -- <class...>
set -uo pipefail
export MSYS2_ARG_CONV_EXCL='*'
export MSYS_NO_PATHCONV=1
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -W)"
TAG="$1"; BIN="$2"; DB="$3"; shift 3
EXTRA=()
while [ "${1:-}" != "--" ]; do EXTRA+=("$1"); shift; done
shift
mkdir -p "$HERE/runs/hibfix-20260822"
OUT="$HERE/runs/hibfix-20260822/$TAG.log"
JDK="$(ls -d "C:/Program Files/Eclipse Adoptium"/jdk-2* 2>/dev/null | head -1)"
DBARGS=(
  -Dhibernate.dialect=org.hibernate.dialect.MySQLDialect
  -Dhibernate.connection.driver_class=com.mysql.cj.jdbc.Driver
  "-Dhibernate.connection.url=jdbc:mysql://localhost/$DB?allowPublicKeyRetrieval=true&useSSL=false"
  -Dhibernate.connection.username=hibernate_orm_test
  -Dhibernate.connection.password=hibernate_orm_test
)
[ "$DB" = "NONE" ] && DBARGS=()
start=$(date +%s%3N)
if [ "$BIN" = "HOTSPOT" ]; then
  "$JDK/bin/java.exe" "@$HERE/common.args" "${EXTRA[@]}" -Djava.awt.headless=true \
     "${DBARGS[@]}" -Dcraton.batch=1 "${RUNNER:-CratonRunner}" "$@" >"$OUT" 2>&1
else
  CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 "$BIN" --java-home "$JDK" --Xmx 1500m "${EXTRA[@]}" \
     "@$HERE/common.args" -Djava.awt.headless=true \
     "${DBARGS[@]}" -Dcraton.batch=1 "${RUNNER:-CratonRunner}" "$@" >"$OUT" 2>&1
fi
rc=$?
end=$(date +%s%3N)
echo "TAG=$TAG rc=$rc wall_ms=$((end-start))"
grep -aE '@@RESULT|@@TESTFAIL' "$OUT" | head -20
