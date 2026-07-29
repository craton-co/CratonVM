# RETIRED — Log4j2 custom-pattern/property failures are not a CratonVM defect

**Status: RETIRED 2026-07-29.** The reported 14 failures are reproduced by
the exact same source fixture on real HotSpot, and CratonVM JIT/no-JIT has
the identical result. This document must not remain in `known-issues`.

## Retirement evidence (2026-07-29)

The originally supplied Spring Boot root was source-pruned: its
`core/spring-boot` module had no `src` tree, compiled test class, or
`cratonvm-test-cp.txt`, so it could not establish fresh VM evidence. A
complete, disposable upstream checkout was therefore pinned to
`spring-projects/spring-boot@6ccca4a5f13cbe120954167ef9150d93526d01d9`,
the revision whose `gradle.properties` exactly matches the supplied
4.1.0-SNAPSHOT dependency versions. `core` test classes and its direct test
classpath were freshly generated with the supplied suite runner.

The following all ran the same two-class selection, including the documented
`Log4j2LoggingSystemPropertiesTests` regression control:

| VM/mode | `Log4j2LoggingSystemPropertiesTests` | `Log4J2LoggingSystemTests` |
|---|---:|---:|
| HotSpot JIT | 3/3, 0 failed | 47/61, **14 failed**, 0 aborted/container failures |
| CratonVM JIT | 3/3, 0 failed | 47/61, **14 failed**, 0 aborted/container failures |
| CratonVM `--nojit` | 3/3, 0 failed | 47/61, **14 failed**, 0 aborted/container failures |

The HotSpot direct runner and the fixture's normal Gradle `:core:spring-boot:test`
task both reproduce the same 14-method signature (plain `log4j2-test.xml`
console pattern and absent file output). Consequently the original
Environment-to-System/Log4j2 hypothesis is not a VM-only root cause, there is
no CratonVM implementation fix to make, and there are no CratonVM residuals
from this document. Any desired behavior change must be resolved in the
Spring Boot/Log4j2 fixture upstream and only re-filed here if a future
CratonVM-vs-HotSpot divergence is demonstrated.

## Symptom

| Module | Class | Failing methods |
|---|---|---|
| `core/spring-boot` | `org.springframework.boot.logging.log4j2.Log4J2LoggingSystemTests` | 14 of 61 (see list below) |

`SBRUNNER_RESULT tests=61 failed=14`. All 14 failures share one of two
shapes, both traceable to the same underlying gap:

**Shape A — console/output tests: the custom pattern segment is simply
absent, output is the fully plain default pattern:**

```java
void correlationLoggingToConsoleWhenHasCorrelationPattern(CapturedOutput output) {
    this.environment.setProperty("logging.pattern.correlation", "%correlationId{spanId(0),traceId(0)}");
    this.loggingSystem.disableSelfInitialization();
    this.loggingSystem.beforeInitialize();
    this.loggingSystem.initialize(this.initializationContext, null, null);
    MDC.setContextMap(Map.of("traceId", "01234567890123456789012345678901", "spanId", "0123456789012345"));
    this.logger.info("Hello world");
    assertThat(getLineWithText(output, "Hello world"))
        .contains(" [0123456789012345-01234567890123456789012345678901] ");
}
```

```
=> java.lang.AssertionError:
Expecting actual:
  "2026-07-29 15:44:52.475  INFO 38212 --- [           main] o.s.b.l.l.Log4J2LoggingSystemTests       : Hello world"
to contain:
  " [0123456789012345-01234567890123456789012345678901] "
```

The actual output is HotSpot's/Log4j2's plain default `PatternLayout` — no
correlation-ID segment, no `[myapp]`/`[mygroup]` segment, nothing — as if
none of `logging.pattern.correlation`/`spring.application.name`/
`spring.application.group` were ever set at all, even though the test set
them on the `Environment` immediately before calling `initialize()`.

Same shape for `spring.application.name`/`spring.application.group`, e.g.:

```
=> java.lang.AssertionError:
Expecting actual:
  "2026-07-29 15:45:26.331  INFO 38212 --- [           main] o.s.b.l.l.Log4J2LoggingSystemTests       : Hello world"
to contain:
  "[myapp] "
```

**Shape B — file-based tests: the log file is never created at all:**

```
=> java.io.UncheckedIOException: Unable to read C:\Users\Victor\AppData\Local\Temp\junit-1785339903891250400\log4j2-test.log
       org.assertj.core.util.Files.contentOf(Files.java:279)
       org.springframework.boot.logging.AbstractLoggingSystemTests.getLineWithText(AbstractLoggingSystemTests.java:95)
       org.springframework.boot.logging.log4j2.Log4J2LoggingSystemTests.applicationNameLoggingToFileWhenHasApplicationNameWithParenthesis(Log4J2LoggingSystemTests.java:676)
     Caused by: java.nio.file.NoSuchFileException
```

Full failure list (`SBRUNNER_RESULT tests=61 failed=14`):
`correlationLoggingToConsoleWhenHasCorrelationPattern`,
`applicationNameLoggingToFileWhenHasApplicationNameWithParenthesis`,
`applicationGroupLoggingToConsoleWhenHasApplicationGroup`,
`applicationNameLoggingToConsoleWhenHasApplicationName`,
`applicationNameLoggingToConsoleWhenHasApplicationNameWithParenthesis`,
`applicationNameLoggingToFileWhenHasApplicationName`,
`correlationLoggingToConsoleWhenExpectCorrelationIdTrueAndNoMdcContent`,
`applicationGroupLoggingToConsoleWhenHasApplicationGroupWithParenthesis`,
`applicationNameLoggingToFileWhenDisabled`,
`applicationGroupLoggingToFileWhenHasApplicationGroup`,
`applicationGroupLoggingToFileWhenHasApplicationGroupWithParenthesis`,
`correlationLoggingToConsoleWhenExpectCorrelationIdTrueAndMdcContent`,
`applicationGroupLoggingToFileWhenDisabled`,
`correlationLoggingToFileWhenExpectCorrelationIdTrueAndMdcContent`.

Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260728/shard2/logs/core_spring-boot.org.springframework.boot.logging.log4j2.Log4J2LoggingSystemTests.out.log`

## Root cause (hypothesis, grounded but not fully bisected)

Spring Boot's Log4j2 integration works by having `LoggingSystemProperties`
(triggered from `LoggingSystem.beforeInitialize()`/`initialize()`) copy
select `Environment` properties
(`spring.application.name`/`spring.application.group`/
`logging.pattern.correlation`/…) into actual JVM `System` properties
(`LOG_APPLICATION_NAME`, `LOG_CORRELATION_PATTERN`, etc.) **before** Log4j2
parses its configuration, because Spring Boot's bundled `log4j2*.xml`
templates reference those values via `${sys:LOG_APPLICATION_NAME}`-style
Log4j2 property lookups inside the `PatternLayout` pattern string. If those
system properties are not visible to Log4j2's own configuration/property
resolver at the moment it builds the `PatternLayout`, the `${sys:...}`
lookups resolve to their Log4j2-side defaults (empty/absent), which — given
the observed output is not just missing the individual token but is Log4j2's
*entire* stock default pattern — suggests the whole `PatternLayout` fell
back to a bundled default configuration rather than Spring Boot's
environment-driven one.

This project's own `docs/internal/fixed-suite-bugs/springboot/properties-keyset-view-not-live-FIXED.md`
documents a related, but distinct, `java.util.Properties`/`System`-properties
view-consistency gap in this exact area (`Log4j2LoggingSystemPropertiesTests`)
that was fixed in stages between 2026-07-24 and 2026-07-26 — and that doc's
own 2026-07-26 "Residual sweep" explicitly re-checked `Log4J2LoggingSystemTests`
and found only **3** failures at that time ("MDC correlation-ID padding
format mismatch in the console/file pattern layout"), calling them
"unrelated" to the `Properties`-view bug it fixed. The current failure count
(14) and shape (complete absence of the custom segment, not a padding/format
mismatch of an otherwise-present segment) is both **larger** and
**qualitatively different** from that 2026-07-26 baseline — this may be a
regression that reintroduces (or is a sibling of) the `Properties`-view gap
that doc fixed, or an entirely new gap in the `Environment`→`System`-property
bridge or Log4j2's own config-reload/property-substitution timing. Not
re-investigated at the code level this session (no build/test execution
performed as part of this task) — flagged as the strongest lead given the
directly-overlapping symptom area and file history, not confirmed.

## Confirming/refuting this hypothesis

1. Re-run `properties-keyset-view-not-live-FIXED.md`'s own regression control
   (`Log4j2LoggingSystemPropertiesTests`) alongside
   `Log4J2LoggingSystemTests` to see whether the `Properties`-view fix has
   itself regressed (e.g. reverted by a later merge) or whether this is a
   genuinely new gap sitting downstream of it (`LoggingSystemProperties`'s
   actual `System.setProperty()` calls, or Log4j2's own property-lookup
   cache/`PropertiesUtil` snapshot).
2. Add a temporary trace (or a minimal repro) confirming whether
   `System.getProperty("LOG_CORRELATION_PATTERN")` (and the
   application-name/group equivalents) is actually set and visible
   immediately after `LoggingSystem.initialize()` runs, to narrow whether the
   gap is in the Spring-side property bridge or in Log4j2's own
   configuration parsing/lookup of those properties under CratonVM.
3. Investigate Shape B (file never created) separately once Shape A is
   understood — it's plausible the same missing-substitution mechanism also
   breaks the `RollingFile`/`File` appender's own `${sys:LOG_FILE}`-style
   path property, so Log4j2 never opens a file at all, but this was not
   confirmed this session.

## Affected classes

| Module | Class |
|---|---|
| `core/spring-boot` | `org.springframework.boot.logging.log4j2.Log4J2LoggingSystemTests` |
