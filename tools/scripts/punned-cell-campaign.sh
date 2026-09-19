#!/bin/bash
# usage: punned-cell-campaign.sh <exe> <tag> <runs> <streams> [extra-vm-args...]
#
# START FOUR CPU SPINNERS FIRST. Without them this measures nothing:
#   for i in 1 2 3 4; do (while :; do :; done) & done
# 600 runs on a quiet host produced ZERO punned cells; 300 runs with four
# spinners produced 17. See
# docs/known-issues/tomcat/punned-sqlchar-rawdata-cell-writer-localized-20260824.md
#
# Paths are the Azure host's (TC_ROOT=/data/cratonvm/apps/tomcat); edit them
# for another fixture.
#
# The `SQLChar.rawData` punned-cell campaign, with the watch's DENOMINATORS.
#
# `[punned-ref]` is printed only by `jit_getfield_impl`, so `--nojit` removes
# the reporter along with the thing it is testing. `accessor_reads_punned` is
# the arm that survives that -- and `accessor_reads` beside it is what makes a
# zero mean "the cell was not there" rather than "nobody looked".
set -u
EXE="$1"; TAG="$2"; RUNS="${3:-100}"; STREAMS="${4:-4}"; shift 4 || shift $#
EXTRA=("$@")
TC_ROOT=/data/cratonvm/apps/tomcat
JAVA_HOME25=/data/toolchain/jdk-25
CLS=org.apache.catalina.servlets.TestWebdavPropertyStore
CP="$TC_ROOT/output/build/webapps/examples/WEB-INF/classes:$(cat $TC_ROOT/.suite/cp-linux-fixed.txt):$TC_ROOT/.build-libs/hamcrest-3.0/hamcrest-3.0.jar"
OUT=/data/ecjrepro/punned/$TAG
rm -rf "$OUT"; mkdir -p "$OUT"
export CRATONVM_REAL_NET_SOCKETS=1 CRATONVM_REAL_AQS=1 CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 CRATONVM_ROOTSNAP_CACHE=1
export CRATONVM_DBG_PUNNED_REF=1
export CRATONVM_DBG_WATCH_PUN="${WATCH_PUN:-SQLChar:1}"
cd "$TC_ROOT" || exit 9

one() {
  local i="$1"
  timeout 300s "$EXE" --java-home "$JAVA_HOME25" --Xmx 2g "${EXTRA[@]}" \
    -Dfile.encoding=UTF-8 -Djava.net.preferIPv4Stack=true \
    -Dtomcat.test.basedir="$TC_ROOT/output/build" \
    -Dtomcat.test.temp="$TC_ROOT/output/test-tmp" \
    -Dtomcat.test.tomcatbuild="$TC_ROOT/output/build" \
    -Dtomcat.test.relaxTiming=true \
    --add-opens java.base/java.lang=ALL-UNNAMED \
    --add-opens java.base/java.io=ALL-UNNAMED \
    --add-opens java.base/java.util=ALL-UNNAMED \
    --add-opens java.base/java.util.concurrent=ALL-UNNAMED \
    -c "$CP" org.junit.runner.JUnitCore "$CLS" > "$OUT/run-$i.log" 2>&1
}

i=0
while [ "$i" -lt "$RUNS" ]; do
  for _ in $(seq 1 "$STREAMS"); do
    [ "$i" -ge "$RUNS" ] && break
    one "$i" &
    i=$((i+1))
  done
  wait
done

sum() { grep -ho "$1=[0-9]*" "$OUT"/run-*.log 2>/dev/null | cut -d= -f2 | paste -sd+ | bc; }
jitcells=$(grep -lE '^\[punned-ref\]' "$OUT"/run-*.log 2>/dev/null | wc -l)
censused=$(grep -l 'WATCH-PUN. EXIT' "$OUT"/run-*.log 2>/dev/null | wc -l)
fails=$(grep -lE '^FAILURES!!!|SIGSEGV|panicked at' "$OUT"/run-*.log 2>/dev/null | wc -l)
echo "PUNNED-ARM $TAG runs=$RUNS censused=$censused runs_with_jit_cell=$jitcells \
reads=$(sum accessor_reads) reads_punned=$(sum accessor_reads_punned) \
stores=$(sum accessor_stores) stores_primitive=$(sum accessor_stores_primitive) \
runs_failed=$fails extra='${EXTRA[*]}'"
echo "PUNNED-DONE $TAG"
