#!/usr/bin/env bash
# one.sh <fqcn> [extra cratonvm args...] — run a single suite class directly,
# from the owning module's directory, with this worktree's binary.
set -u
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SPRING="${SPRING:-/data/data/wt-springsuite8b-20260726/apps/spring-framework}"
JDK="${JDK25:-/home/victor/jdk25}"
BIN="${CRATONVM_BIN:-/data/data/wt-sprbuglist-20260727/localbin/cratonvm-sprbuglist-20260727.bin}"
CLS="$1"; shift
MOD=$(awk -F'\t' -v c="$CLS" '$2==c{print $1; exit}' "$HERE/meta/all-classes.tsv")
[ -n "$MOD" ] || { echo "class not in index: $CLS" >&2; exit 1; }
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
AF="$(mktemp /tmp/af-XXXXXX.txt)"; { echo "-cp"; echo "$CP"; } > "$AF"
# NO heap override here. `run-suite.sh` — the runner that produced every
# published non-passed list — sets none either, so a class triaged through
# one.sh used to run with half the heap of the sweep that flagged it.
# CratonVM's own ergonomic default is min(RAM/4, 4 GiB) (vm-cli
# `ergonomic_default_max_heap`); this line pinned it to 2 GiB, against the
# 8.4 GiB HotSpot takes by its own ergonomics (RAM/4, uncapped) on the
# 31 GiB Azure host — a 4x handicap that manufactures CratonVM-only
# failures. Measured 2026-08-02:
# RequestMappingMessageConversionIntegrationTests scored 141/160 at 2 GiB,
# five of the reported causes being `Java heap space`, and 160/160 — equal
# to HotSpot — at the stock default, same binary, same run.
# Set CRATONVM_DEFAULT_HEAP_MAX_MB in the environment to override.
echo "[one] mod=$MOD  bin=$BIN" >&2
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

cd "$MOD" && exec "$BIN" --java-home "$JDK" "${SPRING_JVM_ARGS[@]}" "$@" "@$AF" KRun "$CLS"
