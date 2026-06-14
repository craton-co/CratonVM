#!/usr/bin/env bash
# After the WildFly build, emit per-module dependency classpaths into each
# module's target/cratonvm-testcp.txt (test scope). Run once; run-wildfly.sh
# reads these. Uses the same default profiles as the build so module set matches.
set -u
WF="/c/craton/cratonvm/apps/wildfly"
MVNW="$WF/mvnw.cmd"
LOG="/c/craton/CratonVM-wildfly/docs/wildfly-suite-bugs/gen-cp.log"
cd "$WF" || exit 1
# -pl testsuite (with submodules) ; build-classpath writes per reactor module.
cmd.exe /c "mvnw.cmd dependency:build-classpath -Dmdep.outputFile=target/cratonvm-testcp.txt -DincludeScope=test -Dmdep.includeTypes=jar -fae -q" > "$LOG" 2>&1
echo "gen-cp rc=$? ; per-module files:"; find "$WF/testsuite" -name cratonvm-testcp.txt | wc -l
