#!/usr/bin/env bash
# Final family-3 validation on the clean-built VM. HotSpot baselines:
#   AnnotationTypeFilterTests 6/6, AssignableTypeFilterTests 4/4,
#   AspectJTypeFilterTests 8/8, DefaultAnnotationMetadataTests 49/49,
#   SimpleAnnotationMetadataTests 49/49
set -u
export MSYS2_ARG_CONV_EXCL='*'; export MSYS_NO_PATHCONV=1
VM=/c/craton/CratonVM-bug06fam/target/debug/cratonvm.exe
JDK='C:\Program Files\Eclipse Adoptium\jdk-25.0.2.10-hotspot'
PCP=$(cygpath -m /c/craton/cratonvm/spring-suite/probe)
AFM=$(cygpath -m /c/craton/cratonvm/spring-suite/.af.txt)
run(){ timeout 320 "$VM" --java-home "$JDK" --stack-dump-on-timeout 0 --add-opens java.base/java.lang=ALL-UNNAMED "@$AFM" KRun "$1" 2>&1 | grep -aE "^RESULT" || echo "NO-RESULT/ABEND: $1"; }
echo "SANITY FindLoaded:"; timeout 60 "$VM" --java-home "$JDK" --add-opens java.base/java.lang=ALL-UNNAMED -cp "$PCP" FindLoaded 2>&1 | head -1
run org.springframework.core.type.AnnotationTypeFilterTests
run org.springframework.core.type.AssignableTypeFilterTests
run org.springframework.core.type.AspectJTypeFilterTests
run org.springframework.core.type.classreading.DefaultAnnotationMetadataTests
run org.springframework.core.type.classreading.SimpleAnnotationMetadataTests
echo "ALLDONE"
