#!/usr/bin/env bash
set -euo pipefail
CP=$(tr -d '\r' < /data/data/spring-framework-shared/spring-expression/build/cratonvm-testcp.txt \
  | sed -e 's#C:\\craton\\cratonvm\\apps\\spring-framework#/data/data/spring-framework-shared#g' \
        -e 's#C:\\Users\\Victor#/home/victor#g' \
        -e 's#\\#/#g' \
        -e 's#;#:#g')
CP="/data/data/spring-suite-runner-shared:$CP"
timeout 300 /data/data/cratonvm-bins/cratonvm-spring-float-double-suffix-20260709-023906 \
  --java-home /data/data/jdk25-real \
  --stack-dump-on-timeout 0 \
  -cp "$CP" \
  KRun \
  org.springframework.expression.spel.LiteralTests \
  org.springframework.expression.spel.ConstructorInvocationTests \
  org.springframework.expression.spel.MethodInvocationTests \
  org.springframework.expression.spel.OperatorTests \
  org.springframework.expression.spel.SpelCompilationCoverageTests \
  org.springframework.expression.spel.SpelDocumentationTests \
  org.springframework.expression.spel.SpelReproTests \
  org.springframework.expression.spel.VariableAndFunctionTests