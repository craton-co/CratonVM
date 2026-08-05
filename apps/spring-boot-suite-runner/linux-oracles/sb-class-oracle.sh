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
# ZipContentTests writes a zip that deliberately exceeds the ZIP SIZE LIMIT.
# One run takes free space on / from ~15 GB down to ~143 MB. Below that headroom
# it fails with `IOException: No space left on device (os error 28)` - and, when
# a file is merely truncated rather than refused, with `Zip64 'End Of Central
# Directory Record' not found`, which reads exactly like zip corruption and was
# filed as a JIT bug. Refuse to emit a verdict we know is about the disk.
case "$FQ" in
  *ZipContentTests*)
    avail_mb=$(df -m --output=avail / | tail -1 | tr -d ' ')
    need_mb=${ZIP_NEED_MB:-16000}
    if [ "${avail_mb:-0}" -lt "$need_mb" ]; then
      echo "SKIP-INSUFFICIENT-DISK ${avail_mb}MB free on /, need >=${need_mb}MB - this class needs ~15GB and fails with ENOSPC (which looks like zip corruption)"
      exit 3
    fi ;;
esac

cd "$SB/$MOD" || exit 9
timeout ${ORACLE_TIMEOUT:-900} "$BIN" --java-home "$JDK" --Xmx ${ORACLE_HEAP:-2g} \
  --stack-dump-on-timeout 0 $CRATONVM_EXTRA_ARGS -Dfile.encoding=UTF-8 -Djava.awt.headless=true \
  -cp "$CP" SbRunner "$FQ" > /tmp/oracle.out 2> /tmp/oracle.err
rc=$?
grep -ohE "OutOfMemoryError.{0,50}|ZipException.{0,50}" /tmp/oracle.out /tmp/oracle.err | head -1
verdict "$rc" "$(grep -o 'SBRUNNER_RESULT.*' /tmp/oracle.out | head -1)" "$FQ"
