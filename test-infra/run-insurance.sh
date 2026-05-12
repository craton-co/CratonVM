#!/usr/bin/env bash
RUSTJVM=/c/Projects/CratonVM/.claude/worktrees/peaceful-sammet-b9d7ca/target/release/rustjvm.exe
JH="C:\Program Files\Eclipse Adoptium\jdk-25.0.2.10-hotspot"
RUSTJVM_DBG_ATHROW=1 RUSTJVM_DISABLE_JIT=1 timeout 60 "$RUSTJVM" --java-home "$JH" --Xmx 1g \
  -Dspring.datasource.url=jdbc:postgresql://localhost:54320/insurance_project \
  -Dspring.datasource.username=sa \
  -Dspring.datasource.password=password \
  --jar "C:\Users\Admin\Yandex.Disk\PRO\JAVA\insurance-backend\target\insurance-0.0.1-SNAPSHOT.jar" 2>&1
