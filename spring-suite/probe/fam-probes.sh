#!/usr/bin/env bash
# Run bug-06 family 3/5/6 reproducers under the freshly-built worktree VM.
# Family 5: Refl5 (reflection-null diff vs HotSpot)
# Family 3: FindLoaded probe + real AnnotationTypeFilterTests via KRun
# Family 6: AnnotationUtilsTests via KRun
set -u
export MSYS2_ARG_CONV_EXCL='*'; export MSYS_NO_PATHCONV=1
VM="${VM:-/c/craton/CratonVM-bug06fam/target/debug/cratonvm.exe}"
JDK='C:\Program Files\Eclipse Adoptium\jdk-25.0.2.10-hotspot'
PROBE=/c/craton/cratonvm/spring-suite/probe
H=$(cygpath -m /c/craton/cratonvm/spring-suite)
CORECP="$H;$PROBE;$(tr -d '\r' < /c/craton/cratonvm/apps/spring-framework/spring-core/build/cratonvm-testcp.txt)"
AF=/c/craton/cratonvm/spring-suite/.af.txt; { echo "-cp"; echo "$CORECP"; } > "$AF"; AFM=$(cygpath -m "$AF")
PCP=$(cygpath -m "$PROBE")

run() { timeout "${2:-90}" "$VM" --java-home "$JDK" --stack-dump-on-timeout 0 "$@" ; }

echo "############ FAMILY 5: Refl5 (CratonVM) ############"
timeout 90 "$VM" --java-home "$JDK" --stack-dump-on-timeout 0 -cp "$PCP" Refl5 2>&1
echo
echo "############ FAMILY 3: FindLoaded (CratonVM) ############"
timeout 90 "$VM" --java-home "$JDK" --stack-dump-on-timeout 0 --add-opens java.base/java.lang=ALL-UNNAMED -cp "$PCP" FindLoaded 2>&1
echo
echo "############ FAMILY 3: AnnotationTypeFilterTests (CratonVM, expect HS: 6/6 OK) ############"
timeout 120 "$VM" --java-home "$JDK" --stack-dump-on-timeout 0 --add-opens java.base/java.lang=ALL-UNNAMED "@$AFM" KRun org.springframework.core.type.AnnotationTypeFilterTests 2>&1 | grep -aE "^(BEGIN|RESULT)" | head
echo
echo "############ FAMILY 6: AnnotationUtilsTests (CratonVM) ############"
timeout 150 "$VM" --java-home "$JDK" --stack-dump-on-timeout 0 --add-opens java.base/java.lang=ALL-UNNAMED "@$AFM" KRun org.springframework.core.annotation.AnnotationUtilsTests 2>&1 | grep -aE "^(BEGIN|RESULT)" | head
echo "############ DONE ############"
