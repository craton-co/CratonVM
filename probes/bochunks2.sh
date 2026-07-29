#!/bin/bash
# bochunks2.sh <tag> — run AotE2EProbe over every chunk. RUNCMD env picks the VM.
set -u
SF=/data/data/wt-springsuite8b-20260726/apps/spring-framework
CP=$(tr -d '\r' < $SF/spring-test/build/cratonvm-testcp.txt)
TAG=${1:-hs}
cd /data/data/aot20260726
for f in bochunks/chunk.[0-9]*; do
  case "$f" in *.log) continue;; esac
  args=$(tr '\n' ' ' < "$f")
  if [ "$TAG" = "hs" ]; then
    timeout 900 /home/victor/jdk25/bin/java -Xmx3g -cp "build:$CP" \
      org.springframework.core.test.tools.ForkedProbeMain AotE2EProbe $args > "$f.$TAG.log" 2>&1
  else
    timeout 2400 "$CRATONVM_BIN" --java-home /home/victor/jdk25 --Xmx 3g -cp "build:$CP" \
      org.springframework.core.test.tools.ForkedProbeMain AotE2EProbe $args > "$f.$TAG.log" 2>&1
  fi
  echo "$(basename $f) rc=$? :: $(grep -a 'PROBE RESULT' $f.$TAG.log | tail -1)"
done
echo "BOCHUNKS-$TAG-DONE"
