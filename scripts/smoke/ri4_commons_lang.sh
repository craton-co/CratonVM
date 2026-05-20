#!/usr/bin/env bash
# RI.4 — Apache Commons Lang unit tests (JUnit platform runner, no server).

source "$(dirname "$0")/common.sh"

MVN_CENTRAL="https://repo1.maven.org/maven2"
V="${COMMONS_LANG_VERSION:-3.14.0}"
COMMONS_JAR="$FIXTURE_CACHE/commons-lang3-$V.jar"
COMMONS_TEST_JAR="$FIXTURE_CACHE/commons-lang3-$V-tests.jar"
JUNIT_LAUNCHER="$FIXTURE_CACHE/junit-platform-console-standalone-1.10.2.jar"

smoke_download "$MVN_CENTRAL/org/apache/commons/commons-lang3/$V/commons-lang3-$V.jar" "$COMMONS_JAR"
smoke_download "$MVN_CENTRAL/org/apache/commons/commons-lang3/$V/commons-lang3-$V-tests.jar" "$COMMONS_TEST_JAR"
smoke_download "$MVN_CENTRAL/org/junit/platform/junit-platform-console-standalone/1.10.2/junit-platform-console-standalone-1.10.2.jar" "$JUNIT_LAUNCHER"

SMOKE_TIMEOUT=600 smoke_run_cratonvm \
    --Xmx 1g \
    --classpath "$COMMONS_JAR:$COMMONS_TEST_JAR:$JUNIT_LAUNCHER" \
    -- org.junit.platform.console.ConsoleLauncher \
       --class-path "$COMMONS_JAR:$COMMONS_TEST_JAR" \
       --select-class org.apache.commons.lang3.StringUtilsTest

# JUnit Console Launcher ends with "[N tests successful]"
smoke_require_signal "tests successful"
