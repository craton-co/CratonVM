#!/usr/bin/env bash
# hibfix-pg.sh <tag> <binary|HOTSPOT> <db> -- <class...>   (Postgres control arm)
set -uo pipefail
export MSYS2_ARG_CONV_EXCL='*'; export MSYS_NO_PATHCONV=1
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -W)"
TAG="$1"; BIN="$2"; DB="$3"; shift 3; shift
mkdir -p "$HERE/runs/hibfix-20260822"
OUT="$HERE/runs/hibfix-20260822/$TAG.log"
JDK="$(ls -d "C:/Program Files/Eclipse Adoptium"/jdk-2* 2>/dev/null | head -1)"
DBARGS=(-Dhibernate.dialect=org.hibernate.dialect.PostgreSQLDialect
  -Dhibernate.connection.driver_class=org.postgresql.Driver
  "-Dhibernate.connection.url=jdbc:postgresql://localhost:5433/$DB"
  -Dhibernate.connection.username=hibernate_orm_test -Dhibernate.connection.password=hibernate_orm_test)
if [ "$BIN" = "HOTSPOT" ]; then
  ARGS=("@$HERE/common.args" -Djava.awt.headless=true "${DBARGS[@]}" -Dcraton.batch=1 "${RUNNER:-CratonRunner}" "$@"); EXE="$JDK/bin/java.exe"
else
  ARGS=(--java-home "$JDK" --Xmx 1500m "@$HERE/common.args" -Djava.awt.headless=true "${DBARGS[@]}" -Dcraton.batch=1 "${RUNNER:-CratonRunner}" "$@"); EXE="$BIN"
fi
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 powershell -NoProfile -ExecutionPolicy Bypass -File "$HERE/hibfix-cpu.ps1" "$OUT" "$EXE" "${ARGS[@]}"
echo "TAG=$TAG"; grep -aE '@@RESULT|@@TESTFAIL' "$OUT" | head -5
