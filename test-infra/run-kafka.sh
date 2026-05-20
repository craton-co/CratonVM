#!/usr/bin/env bash
# Run Apache Kafka 4.2.0 broker (Scala 2.13) under CratonVM.
# Note: classpath uses Unix ':' separator (CratonVM is path-aware).
set -u
WT=/c/Projects/CratonVM/.claude/worktrees/peaceful-sammet-b9d7ca
CRATONVM="$WT/target/release/cratonvm.exe"
JH="C:\\Program Files\\Eclipse Adoptium\\jdk-25.0.2.10-hotspot"
KA=/c/Projects/CratonVM/apps/kafka_2.13-4.2.0
CP=$(ls "$KA"/libs/*.jar | tr '\n' ':' | sed 's/:$//')
CRATONVM_DISABLE_JIT=1 timeout 60 "$CRATONVM" --java-home "$JH" --Xmx 1g \
  -Dkafka.logs.dir="$KA/logs" \
  --classpath "$CP" \
  kafka.Kafka "$KA/config/server.properties" "$@"
