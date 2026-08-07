#!/usr/bin/env bash
# onem.sh <fqcn> <method> — run ONE test method under CratonVM, same classpath
# and JVM args as one.sh. Cheap smoke signal when the full class is too
# expensive to run on a loaded shared box.
set -u
HERE=/data/data/wt-aotsegv-20260806/apps/spring-suite-runner
SPRING="${SPRING:-/data/data/wt-springsuite8b-20260726/apps/spring-framework}"
JDK="${JDK25:-/home/victor/jdk25}"
BIN="${CRATONVM_BIN:?set CRATONVM_BIN}"
CLS="$1"; MTH="$2"; shift 2
MOD=$(awk -F'\t' -v c="$CLS" '$2==c{print $1; exit}' "$HERE/meta/all-classes.tsv")
[ -n "$MOD" ] || { echo "class not in index: $CLS" >&2; exit 1; }
CP="$HERE:$(tr -d '\r' < "$MOD/build/cratonvm-testcp.txt")"
AF="$(mktemp /tmp/afm-XXXXXX.txt)"; { echo "-cp"; echo "$CP"; } > "$AF"
ARGS=(
  --add-opens=java.base/java.lang=ALL-UNNAMED
  --add-opens=java.base/java.util=ALL-UNNAMED
  -Djava.awt.headless=true
  -Dio.netty.leakDetection.level=paranoid
  -Djunit.platform.discovery.issue.severity.critical=INFO
)
case "$MOD" in
  */spring-test) ARGS+=(-Djunit.vintage.discovery.issue.reporting.enabled=false) ;;
esac
cd "$MOD" && exec "$BIN" --java-home "$JDK" "${ARGS[@]}" "$@" "@$AF" KRunM "$CLS" "$MTH"
