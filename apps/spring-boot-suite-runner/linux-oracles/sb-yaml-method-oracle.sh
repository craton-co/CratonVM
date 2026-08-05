#!/bin/bash
BIN=${CRATONVM_BIN:-/tmp/cratonvm-zj2}
JDK=${ORACLE_JDK:-/data/jdk25-real-20260717/jdk-25.0.3+9}
SB=${SB_ROOT:-/data/data/springboot-jsonreader-deprecation-20260718}
MOD=core/spring-boot
. /tmp/verdict.inc
[ -x "$BIN" ] || { echo "ORACLE-ERROR missing binary $BIN"; exit 9; }
CP="$SB/sb-runner:$(cat $SB/$MOD/build/cratonvm-test-cp.txt)"
cd "$SB/$MOD" || exit 9
timeout ${ORACLE_TIMEOUT:-900} "$BIN" --java-home "$JDK" --Xmx ${ORACLE_HEAP:-2g} \
  --stack-dump-on-timeout 0 $CRATONVM_EXTRA_ARGS -Dfile.encoding=UTF-8 -Djava.awt.headless=true \
  -cp "$CP" OneMethodRunner org.springframework.boot.env.OriginTrackedYamlLoaderTests \
  canLoadFilesBiggerThan3Mb > /tmp/yaml.out 2> /tmp/yaml.err
rc=$?
grep -ohE "expected .{0,40}|line [0-9]+, column [0-9]+" /tmp/yaml.out | head -1
verdict "$rc" "$(grep -o 'SBRUNNER_RESULT.*' /tmp/yaml.out | head -1)" "yaml"
