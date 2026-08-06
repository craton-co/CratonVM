#!/usr/bin/env bash
# hs.sh <fqcn> — run one suite class on HotSpot, from its module directory.
set -u
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MOD=$(awk -F'\t' -v c="$1" '$2==c{print $1; exit}' "$HERE/meta/all-classes.tsv")
[ -n "$MOD" ] || { echo "not in index: $1" >&2; exit 1; }
# The classpath is `cratonvm-testcp.txt` VERBATIM. That file is a dump of
# Gradle's own `sourceSets.test.runtimeClasspath` (see
# dump-testcp.init.gradle), so it already leads with the module's test output
# and carries its main output in Gradle's position. Prepending
# `build/classes/java/main` ahead of it — which this script used to do — puts
# the module's MAIN package directories in front of its TEST ones and silently
# changes what classpath-order-sensitive lookups resolve to. Measured
# 2026-08-01: that alone failed MockServletContextTests.getResourcePaths and
# PathMatchingResourcePatternResolverTests
# .usingClasspathStarProtocolWithWildcardInPatternAndEndingInSlash on HOTSPOT,
# both of which pass 19/19 and 22/22 with the classpath left alone.
CP="$HERE:$(tr -d '\r' < "$MOD/build/cratonvm-testcp.txt")"
# Spring's own Gradle test task applies these to EVERY test JVM
# (spring-framework buildSrc/src/main/java/org/springframework/build/
# TestConventions.java). Omitting them is not a neutral simplification — it
# manufactures failures that HotSpot does not have:
#   * without --add-opens=java.base/java.lang, Spring-CGLIB's ReflectUtils
#     cannot reach ClassLoader.defineClass, so EVERY CGLIB-generated class
#     fails with "No compatible defineClass mechanism detected". Measured
#     2026-08-01: BshScriptFactoryTests 5/18 and Spr15042Tests 0/1 on HOTSPOT
#     without the flag, 18/18 and 1/1 with it (and Gradle agrees).
#   * -Xshare:off is HotSpot-only (CratonVM has no CDS archive) and is added
#     to the hotspot mode alone.
SPRING_JVM_ARGS=(
  --add-opens=java.base/java.lang=ALL-UNNAMED
  --add-opens=java.base/java.util=ALL-UNNAMED
  -Djava.awt.headless=true
  -Dio.netty.leakDetection.level=paranoid
  -Djunit.platform.discovery.issue.severity.critical=INFO
)

# Modules may add their OWN test system properties on top of
# TestConventions, and spring-test does. `spring-test/spring-test.gradle`
# sets `junit.vintage.discovery.issue.reporting.enabled=false` with the
# comment "we disable reporting of the 'deprecated' discovery issue,
# because that would otherwise fail the build" — spring-test is the module
# that deliberately keeps the JUnit Vintage engine (it runs JUnit 4 tests),
# and Vintage reports its own deprecation as an INFO discovery issue, which
# TestConventions' `discovery.issue.severity.critical=INFO` then promotes to
# critical. Measured 2026-08-02: without this, `test.context.aot
# .AotIntegrationTests#endToEndTests` fails on HOTSPOT with
# `DiscoveryIssueException: TestEngine with ID 'junit-vintage' encountered a
# critical issue during test discovery`, and passes with it.
case "$MOD" in
  */spring-test)
    SPRING_JVM_ARGS+=(-Djunit.vintage.discovery.issue.reporting.enabled=false) ;;
esac

CLS="$1"; shift
# The Azure Linux host's JDK stays the default so existing invocations are
# unchanged; `HS_JAVA` lets the same script point at a JDK installed somewhere
# else. HotSpot IS the oracle for this suite, so it has to be runnable wherever
# the comparison is being made.
#
# This is NOT enough to run the script on a Windows checkout: `CP` is joined
# with `:`, which a Windows JVM reads as part of a drive letter rather than as a
# separator, and the paths themselves are Git-Bash-style. Windows runs need
# their own wrapper.
cd "$MOD" && exec timeout "${HS_TO:-300}" "${HS_JAVA:-/home/victor/jdk25/bin/java}" \
  "${SPRING_JVM_ARGS[@]}" -Xshare:off "$@" -cp "$CP" KRun "$CLS"
