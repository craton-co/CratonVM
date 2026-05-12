#!/usr/bin/env bash
# Run SportMe (Spring Boot 2.0.3) under CratonVM with local test-infra credentials.
# Overrides hardcoded 195.24.66.69 from application-localdev.yml to localhost.
set -u
WT=/c/Projects/CratonVM/.claude/worktrees/peaceful-sammet-b9d7ca
RUSTJVM="$WT/target/release/rustjvm.exe"
JH="C:\\Program Files\\Eclipse Adoptium\\jdk-25.0.2.10-hotspot"
JAR="C:\\Users\\Admin\\Yandex.Disk\\PRO\\JAVA\\SportMe-master\\target\\sportme-backend.jar"
RUSTJVM_DISABLE_JIT=1 timeout 60 "$RUSTJVM" --java-home "$JH" --Xmx 1g \
  -Dredis.url=redis://localhost:6379 \
  -Dspring.redis.host=localhost \
  -Dspring.redis.port=6379 \
  -Dspring.datasource.url=jdbc:postgresql://localhost:5432/sportme \
  -Dspring.datasource.username=sportme \
  -Dspring.datasource.password=weg5jas9kMglewq \
  --jar "$JAR" "$@"
