#!/usr/bin/env bash
CRATONVM=/c/Projects/CratonVM/.claude/worktrees/peaceful-sammet-b9d7ca/target/release/cratonvm.exe
JH="C:\Program Files\Eclipse Adoptium\jdk-25.0.2.10-hotspot"
CRATONVM_DBG_ATHROW=1 CRATONVM_DISABLE_JIT=1 timeout 60 "$CRATONVM" --java-home "$JH" --Xmx 1g \
  -Dspring.datasource.url=jdbc:postgresql://localhost:54320/insurance_project \
  -Dspring.datasource.username=sa \
  -Dspring.datasource.password=password \
  --jar "C:\Users\Admin\Yandex.Disk\PRO\JAVA\insurance-backend\target\insurance-0.0.1-SNAPSHOT.jar" 2>&1
