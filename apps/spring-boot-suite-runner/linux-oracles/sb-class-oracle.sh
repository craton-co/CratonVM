#!/bin/bash
# usage: ziporacle.sh <SimpleName|FQCN>
# env:   CRATONVM_BIN CRATONVM_EXTRA_ARGS ORACLE_HEAP ORACLE_TIMEOUT
BIN=${CRATONVM_BIN:-/tmp/cratonvm-zj2}
JDK=${ORACLE_JDK:-/data/jdk25-real-20260717/jdk-25.0.3+9}
SB=${SB_ROOT:-/data/data/springboot-jsonreader-deprecation-20260718}
. /tmp/verdict.inc
C="$1"
case "$C" in
  *.*)             FQ="$C" ;;
  ZipContentTests) FQ="org.springframework.boot.loader.zip.$C" ;;
  NestedJarFileTests|SecurityInfoTests) FQ="org.springframework.boot.loader.jar.$C" ;;
  ImagePackagerTests|RepackagerTests)   FQ="org.springframework.boot.loader.tools.$C" ;;
  *) echo "ORACLE-ERROR unknown class $C"; exit 9 ;;
esac
case "$FQ" in
  *loader.tools.*)             MOD=loader/spring-boot-loader-tools ;;
  *loader.jar.*|*loader.zip.*) MOD=loader/spring-boot-loader ;;
  *)                           MOD=core/spring-boot ;;
esac
[ -x "$BIN" ] || { echo "ORACLE-ERROR missing binary $BIN"; exit 9; }
[ -f "$SB/$MOD/build/cratonvm-test-cp.txt" ] || { echo "ORACLE-ERROR missing cp $MOD"; exit 9; }
CP="$SB/sb-runner:$(cat $SB/$MOD/build/cratonvm-test-cp.txt)"
cd "$SB/$MOD" || exit 9
timeout ${ORACLE_TIMEOUT:-900} "$BIN" --java-home "$JDK" --Xmx ${ORACLE_HEAP:-2g} \
  --stack-dump-on-timeout 0 $CRATONVM_EXTRA_ARGS -Dfile.encoding=UTF-8 -Djava.awt.headless=true \
  -cp "$CP" SbRunner "$FQ" > /tmp/oracle.out 2> /tmp/oracle.err
rc=$?
grep -ohE "OutOfMemoryError.{0,50}|ZipException.{0,50}" /tmp/oracle.out /tmp/oracle.err | head -1
verdict "$rc" "$(grep -o 'SBRUNNER_RESULT.*' /tmp/oracle.out | head -1)" "$FQ"
