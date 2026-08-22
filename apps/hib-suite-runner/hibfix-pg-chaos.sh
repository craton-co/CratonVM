#!/usr/bin/env bash
# hibfix-pg-chaos.sh <tag> <binary> <gcflag> <rounds> -- <class...>
# Runs the classes repeatedly while a background loop degrades the Postgres
# server (terminates every backend, then bounces max_connections pressure).
# This is the "degraded database server" condition the JSON/XML SIGSEGV page
# names as its standing hypothesis but never actually recreated.
set -uo pipefail
export MSYS2_ARG_CONV_EXCL='*'; export MSYS_NO_PATHCONV=1
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -W)"
TAG="$1"; BIN="$2"; GCFLAG="$3"; ROUNDS="$4"; shift 4; shift
JDK="$(ls -d "C:/Program Files/Eclipse Adoptium"/jdk-2* 2>/dev/null | head -1)"
mkdir -p "$HERE/runs/hibfix-20260822"
( while [ -f "$HERE/runs/hibfix-20260822/.chaos-$TAG" ]; do
    docker exec -i hibfix-pg psql -U hibernate_orm_test -d hibernate_orm_test -c \
      "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE pid <> pg_backend_pid() AND datname='hibernate_orm_test';" >/dev/null 2>&1
    sleep 3
  done ) &
CHAOS=$!
touch "$HERE/runs/hibfix-20260822/.chaos-$TAG"
for r in $(seq 1 "$ROUNDS"); do
  OUT="$HERE/runs/hibfix-20260822/$TAG-r$r.log"
  CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 "$BIN" $GCFLAG --java-home "$JDK" --Xmx 1500m \
    "@$HERE/common.args" -Djava.awt.headless=true \
    -Dhibernate.dialect=org.hibernate.dialect.PostgreSQLDialect \
    -Dhibernate.connection.driver_class=org.postgresql.Driver \
    -Dhibernate.connection.url=jdbc:postgresql://localhost:5433/hibernate_orm_test \
    -Dhibernate.connection.username=hibernate_orm_test -Dhibernate.connection.password=hibernate_orm_test \
    -Dcraton.batch=1 CratonRunner "$@" >"$OUT" 2>&1
  rc=$?
  crash=$(grep -ac "fatal error has been detected" "$OUT" 2>/dev/null; true)
  echo "round=$r rc=$rc crash=$crash results=$(grep -ac "@@RESULT" "$OUT" 2>/dev/null; true)"
  if [ "$crash" != "0" ]; then echo "!!! CRASH in $OUT"; break; fi
done
rm -f "$HERE/runs/hibfix-20260822/.chaos-$TAG"
wait $CHAOS 2>/dev/null
