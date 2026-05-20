#!/usr/bin/env bash
# Run Keycloak 26.6.1 (Quarkus) under CratonVM for Quarkus error gathering.
set -u
WT=/c/Projects/CratonVM/.claude/worktrees/peaceful-sammet-b9d7ca
CRATONVM="$WT/target/release/cratonvm.exe"
JH="C:\\Program Files\\Eclipse Adoptium\\jdk-25.0.2.10-hotspot"
KC=/c/Projects/CratonVM/apps/keycloak-26.6.1
cd "$KC"
CRATONVM_DISABLE_JIT=1 timeout 60 "$CRATONVM" --java-home "$JH" --Xmx 2g \
  -Dkc.config.built=true \
  -Dkc.home.dir="$KC" -Djboss.server.config.dir="$KC/conf" \
  -Djava.util.concurrent.ForkJoinPool.common.threadFactory=io.quarkus.bootstrap.forkjoin.QuarkusForkJoinWorkerThreadFactory \
  --jar lib/quarkus-run.jar \
  start-dev "$@"
