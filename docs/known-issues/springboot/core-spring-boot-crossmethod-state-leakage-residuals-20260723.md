# Possible cross-test-method state leakage within a single SbRunner process (pattern observation)

**Status: OPEN — found 2026-07-23 (pattern-level hypothesis across 3 classes, not a single confirmed mechanism)**

## Symptom

Three unrelated-looking failures share the same *shape*: a value that a
test explicitly sets (a system property, or an object's own state) right
before exercising the code under test is either ignored (a stale
default/absent value observed instead) or polluted by a value from a
sibling test method that ran earlier in the same class/process.

**1. `DefaultLogbackConfigurationTests.fileLogCharsetShouldUseSystemPropertyIfSet()`**
```
expected: "ISO-8859-1"
 but was: null
```
sets `System.setProperty("FILE_LOG_CHARSET", "ISO-8859-1")` then calls
`new DefaultLogbackConfiguration(null).apply(...)` and reads
`loggerContext.getProperty("FILE_LOG_CHARSET")` — comes back `null`. The
*sibling* test `consoleLogCharsetShouldUseSystemPropertyIfSet()`, which
uses the exact same `withSystemProperty(...)` helper and the exact same
`putProperty(config, NAME, "${NAME:-default}")` mechanism in
`DefaultLogbackConfiguration.java:111` (CONSOLE) vs `:115` (FILE), **passes**.

**2. `Log4j2LoggingSystemPropertiesTests.appliesWithLogFile()`**
```
Expecting map: {..., "LOG4J2_ROLLINGPOLICY_MAX_FILE_SIZE"="52428800", ...}
to contain entries: ["LOG4J2_ROLLINGPOLICY_MAX_FILE_SIZE"="26214400"]
but the following map entries had different values:
  ["LOG4J2_ROLLINGPOLICY_MAX_FILE_SIZE"="52428800" (expected: "26214400")]
```
the test configures a 25 MiB (`26214400` byte) rolling-policy max file
size; the system property map that comes back instead shows `52428800`
(50 MiB) — a *different, fixed* value, not the one under test.
`appliesLog4j2RollingPolicyPropertiesWithDefaults()` (same class) also
fails on a related but distinct assertion (an env-derived
`LOG4J2_ROLLINGPOLICY_TIME_MODULATE` value it expects absent).

**3. `LogbackConfigurationAotContributionTests.contributionOfBasicModel()`**
and `.contributionOfBasicModelThatMatchesExistingModel()`
```
Expecting actual:
  [com.example.Alpha, ch.qos.logback.core.model.Model, com.example.Bravo, java.util.ArrayList, java.lang.Integer, java.lang.Boolean]
to contain exactly in any order:
  [ch.qos.logback.core.model.Model, java.util.ArrayList, java.lang.Boolean, java.lang.Integer]
but the following elements were unexpected:
  [com.example.Alpha, com.example.Bravo]
```
the test builds a fresh, empty `ch.qos.logback.core.model.Model` (no
`com.example.Alpha`/`Bravo` involved at all) and asserts the AOT
`ReflectionHints` contain exactly the 4 types a bare `Model` needs — but
two extra types (`com.example.Alpha`, `com.example.Bravo`) show up, which
strongly look like fixture classes referenced by a *different* test method
elsewhere in this file (a richer model with custom typed fields).

Full logs:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260723/shard3/logs/core_spring-boot.org.springframework.boot.logging.logback.DefaultLogbackConfigurationTests.out.log`
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260723/shard6/logs/core_spring-boot.org.springframework.boot.logging.log4j2.Log4j2LoggingSystemPropertiesTests.out.log`
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260723/shard6/logs/core_spring-boot.org.springframework.boot.logging.logback.LogbackConfigurationAotContributionTests.out.log`

## Root cause

**Not confirmed as a single mechanism** — these three classes exercise
completely different subsystems (Logback's `OptionHelper`/Joran variable
substitution, Log4j2's `LoggingSystemProperties` binding, and Spring's AOT
`RuntimeHints`/`ReflectionHints` collection), so this is flagged as a
*pattern observation*, not a root-caused single bug: in every case, a
value that should be freshly computed from this test method's own setup
instead reflects either a stale default or another test method's state.
`SbRunner` (`apps/spring-boot/sb-runner/SbRunner.java`) runs an entire test
class's methods in one JVM process via a single `Launcher.execute()`, so
any of these subsystems caching a resolved value in a JVM-static (rather
than per-`LoggerContext`/per-`RuntimeHints`-instance) location would
produce exactly this shape of leakage between sibling test methods in the
same class, without needing any CratonVM involvement at all in the
caching itself — though if the caching key or invalidation logic depends
on identity/equality machinery CratonVM handles differently than HotSpot,
that would make CratonVM the actual differentiator even though the cache
itself is real Logback/Log4j2/Spring code.

**What would confirm/refute:** rerun each of the three classes with only a
single method selected (`-Parallel 1`, one test at a time, fresh process
per method) — if all three failures disappear in isolation, that confirms
cross-method state leakage within the shared process as the mechanism, and
narrows the next step to finding what specifically differs between
HotSpot's and CratonVM's handling of the caching/identity mechanism each
subsystem uses (Logback: `ch.qos.logback.core.util.OptionHelper`'s property
substitution; Log4j2: `LoggingSystemProperties`'s environment/system-property
binding; Spring AOT: `RuntimeHints`/`ReflectionHints` — though the latter
two use fresh `new RuntimeHints()`/binder instances per test method
according to their source, so a leak there would be more surprising and
worth particular attention).

## Affected classes

| Module | Class |
|---|---|
| core/spring-boot | org.springframework.boot.logging.logback.DefaultLogbackConfigurationTests (1 of 3 failures: `fileLogCharsetShouldUseSystemPropertyIfSet`) |
| core/spring-boot | org.springframework.boot.logging.log4j2.Log4j2LoggingSystemPropertiesTests (2 of 2 failures: `appliesLog4j2RollingPolicyPropertiesWithDefaults`, `appliesWithLogFile`) |
| core/spring-boot | org.springframework.boot.logging.logback.LogbackConfigurationAotContributionTests (2 of 2 failures: `contributionOfBasicModel`, `contributionOfBasicModelThatMatchesExistingModel`) |

`DefaultLogbackConfigurationTests` has one other, unrelated failure in the
same run (`consoleLogCharsetShouldUseConsoleCharsetIfConsoleAvailable`) —
see `core-spring-boot-console-ttystatus-missing-native-20260723.md`. Its
remaining failure (`consoleLogCharsetShouldDefaultToUtf8WhenConsoleIsNull`)
is listed in `core-spring-boot-uncategorized-residuals-20260723.md`
pending confirmation of whether it belongs in this cluster too (it uses a
Mockito *spy*, not a system property, so the mechanism may differ).
