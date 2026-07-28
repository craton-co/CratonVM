# FIXED — `core/spring-boot` 2026-07-23 rerun — uncategorized individual residuals

**Status: FIXED 2026-07-28.** Each item below is an independent,
distinct failure; they are bundled into one doc because each individual
investigation stayed at hypothesis level (none pinned to a CratonVM
file:line this session) rather than because they share a root cause.**

## Superseding closure — FIXED 2026-07-28

All eight affected classes now pass from fresh processes on the Windows
Spring Boot fixture. CratonVM passes the complete list in both JIT and
`--nojit` modes: **8/8 classes, 55 started tests, 0 failed, 0 aborted, and
0 failed containers** in each mode. `ProcessInfoTests` has its expected
single availability-dependent virtual-thread skip in both CratonVM modes
and in the HotSpot control. The HotSpot JIT control also passes all eight
classes (55 started tests, zero failures).

The final apparent residual, `JavaLoggingSystemTests`, was not a VM
failure. This host exports `LOG_FORMAT=json`, which Spring Boot's JUL
formatter intentionally treats as an application override. That made both
HotSpot and CratonVM fail the tests that assert Spring Boot's default
formatter output. The suite runner now removes only that ambient override
from each child process, preserving explicit Java system properties used by
tests. Its `-AllModes` pathing JARs are also mode-local so concurrent JIT
and no-JIT runs cannot race while replacing the same wrapper JAR.

## 1. `ApplicationHomeTests.whenSourceClassIsProvidedWithSpaceInItsPathThenApplicationHomeReflectsItsLocation()`

```
=> java.lang.ClassNotFoundException
```

Full log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260723/shard1/logs/core_spring-boot.org.springframework.boot.system.ApplicationHomeTests.out.log`

The test method name implies it exercises a source class located under a
directory path containing a literal space. `ApplicationHome.findSource(URL
location)` (`apps/spring-boot/core/spring-boot/src/main/java/org/springframework/boot/system/ApplicationHome.java:124-129`)
does `new File(location.toURI())`, which requires correct `%20`
percent-encoding/decoding round-tripping through the `URL`/`URI` pair. The
failure is a `ClassNotFoundException`, not a `URISyntaxException`, so the
break is more likely happening earlier, in the test's own setup step that
loads a helper class from a synthetically-constructed space-containing
classpath directory (i.e. `URLClassLoader`/CratonVM's classpath resolution
not correctly decoding `%20` back to a literal space when resolving a
`file:` URL classpath entry to load a `.class` file) rather than inside
`ApplicationHome` itself. This is the same *family* of bug as the
already-fixed `File(URI)` percent-decoding gap (see
`../../internal/fixed-suite-bugs/springboot/staticresourcejarstests-jar-url-handling-cluster-FIXED.md`)
but for a directory classpath entry rather than a jar/resource URL — not
confirmed to be the same code path or a regression of that fix.

## 2. `SpringApplicationShutdownHookTests.runWhenContextIsBeingClosedInAnotherThreadWaitsUntilContextIsInactive()`

```
=> org.awaitility.core.ConditionTimeoutException: Lambda expression ... expected the predicate to return <true> but it returned <false> for input of <WAITING> within 30 seconds.
```

Full log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260723/shard1/logs/core_spring-boot.org.springframework.boot.SpringApplicationShutdownHookTests.out.log`

Awaitility polls (for 30s) for a background thread's `Thread.getState()` to
report a specific state; the poll condition never observes the expected
state and times out. This is consistent with — though not confirmed to be
the same as — the project's already-tracked thread-state fidelity gap
noted in `thread-dump-endpoint-jmx-threadinfo-fidelity.md`'s history ("the
liveness portion of `ThreadDumpEndpointTests` is fixed, but its separate
JMX lock/monitor diagnostic data remains incomplete"), which points at
`Thread.getState()`/monitor-state reporting as a known-imperfect area. Not
independently verified this session (no CPU sampling or thread dump was
taken; per the project's own guidance, a CPU sample alone can't distinguish
a real hang from a slow-but-progressing wait, so this needs a dedicated
repro to confirm rather than a code-reading-only pass).

## 3. `ConfigTreeConfigDataLocationResolverTests.resolveReturnsConfigVolumeMountLocation()`

```
Expecting actual: ["config tree [C:\etc\config\]"]
to contain exactly (and in same order): ["config tree [C:\etc\config]"]
```

Full log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260723/shard2/logs/core_spring-boot.org.springframework.boot.context.config.ConfigTreeConfigDataLocationResolverTests.out.log`

The test resolves `configtree:/etc/config/` (note trailing slash in the
location) and compares the resulting resource's `toString()` against
`"config tree [" + new File("/etc/config").getAbsolutePath() + "]"` (no
trailing slash, since `new File(...)`'s `getAbsolutePath()` normalizes it
away). CratonVM's resolved resource keeps a trailing backslash. Not traced
to a specific `ConfigTreeConfigDataLocationResolver` or `File`/`Path`
normalization call this session — worth checking whether the resolver
builds its display string from the raw location string (preserving the
trailing `/` verbatim after Windows separator conversion) rather than from
a normalized `File`/`Path`, in which case this may be a Spring-source
issue rather than a CratonVM one; alternatively, `File.getAbsolutePath()`/
path normalization itself may differ from HotSpot's trailing-separator
stripping on Windows.

## 4. `ProcessInfoTests.memoryInfoIsAvailable()` / `.virtualThreadsInfoIfAvailable()`

```
memoryInfoIsAvailable(): Expecting actual: 0L to be greater than: 0L
virtualThreadsInfoIfAvailable(): Expecting actual not to be null
```

Full log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260723/shard4/logs/core_spring-boot.org.springframework.boot.info.ProcessInfoTests.out.log`

`ProcessInfo.MemoryInfo` (`apps/spring-boot/core/spring-boot/src/main/java/org/springframework/boot/info/ProcessInfo.java:261-326`)
reads `ManagementFactory.getMemoryMXBean().getHeapMemoryUsage().getUsed()`
— comes back `0`. `ProcessInfo.getVirtualThreads()` (`ProcessInfo.java:116-134`)
requires `jdk.management.VirtualThreadSchedulerMXBean` to be present *and*
`ManagementFactory.getPlatformMXBean(...)` to return a working bean with
working `getMountedVirtualThreadCount`/`getQueuedVirtualThreadCount`/
`getParallelism`/`getPoolSize` methods (any exception anywhere in that
chain is swallowed and the method returns `null`) — comes back `null`.
Both point at incomplete `java.lang.management`/`ManagementFactory` MXBean
support (heap-usage reporting and the virtual-thread-scheduler MXBean
specifically) rather than anything reachable via source reading in this
repo (the MXBean implementations are native/JDK-internal, not CratonVM
Spring Boot application code) — not pinned to a specific native
registration gap this session.

## 5. `LogbackRuntimeHintsTests.doesNotRegisterHintsWhenLoggerContextIsNotAvailable()`

```
Expecting empty but was: [TypeHint[type=ch.qos.logback.classic.pattern.SyslogStartConverter], ... 10 entries ...]
```

Full log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260723/shard4/logs/core_spring-boot.org.springframework.boot.logging.logback.LogbackRuntimeHintsTests.out.log`

The test calls `new LogbackRuntimeHints().registerHints(hints,
ClassLoader.getPlatformClassLoader())` — deliberately passing the
*platform* classloader, which on real HotSpot cannot see
`ch.qos.logback.classic.LoggerContext` (Logback is an application-classpath
dependency, not on the platform/bootstrap tier), so the hints registrar's
presence check should short-circuit and register nothing. CratonVM
registers the full 10-entry hint set anyway, implying its
`ClassLoader.getPlatformClassLoader()` either aliases the application
classloader or otherwise doesn't provide the isolation HotSpot's does (or
`ClassUtils.isPresent(name, classLoader)` — the check
`LogbackRuntimeHints` almost certainly uses — doesn't honor the passed-in
classloader argument and instead consults the thread context classloader).
Not traced further this session.

## 6. `JavaLoggingSystemTests` (9 of 12 tests failing)

```
testNonDefaultConfigLocation(): expected to contain "INFO: Hello", was "INFO [...ClassName] Hello world\n"
testSystemPropertyInitializesFormat(): expected to contain "1234 INFO [", was default format
noFile()/withFile(): expected NOT to contain "Hidden", but it does (log level filtering not applied)
testCustomFormatter(): expected custom "???? INFO [" prefix, got default format
getLoggerConfiguration()/getLoggerConfigurations(): NullPointerException — "logger" is null at JavaLoggingSystem.getEffectiveLevel(JavaLoggingSystem.java:163)
setLevel()/setLevelToNull(): expected 1 log line at a given level, got 0
```

Full log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260723/shard7/logs/core_spring-boot.org.springframework.boot.logging.java.JavaLoggingSystemTests.out.log`

Every failure is consistent with a single underlying cause: CratonVM's
`java.util.logging.LogManager` custom-configuration-file loading
(`LogManager.readConfiguration(InputStream)`, driven by
`JavaLoggingSystem`'s config-location handling) never actually taking
effect. All output stays in one fixed default format
(`"INFO [fully.qualified.ClassName] message"`) regardless of which custom
`logging.properties`/formatter/level the test configures, log-level
filtering doesn't apply (`noFile`/`withFile` expect a `FINE`-or-lower
"Hidden" line to be suppressed and it isn't), and `Logger.getLogger(name)`
returns `null` for loggers the test expects the LogManager to have
registered from its config. This is a plausible, coherent single-root-cause
hypothesis but was not traced to a specific CratonVM native or
`LogManager` override this session — worth a dedicated follow-up checking
whether `LogManager.readConfiguration` is registered/functional at all on
CratonVM.

## 7. `MessageSourceMessageInterpolatorIntegrationTests.unknown()`

```
Expecting actual: ["null"]
to contain exactly (and in same order): ["{unknown}"]
```

Full log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260723/shard6/logs/core_spring-boot.org.springframework.boot.validation.MessageSourceMessageInterpolatorIntegrationTests.out.log`

Bean Validation's message-interpolation contract: an unresolvable message
key (`"unknown"`, not a real message code) should fall back to the
*original, unresolved* placeholder text (`"{unknown}"`) rather than being
stringified. The actual result is the **literal string `"null"`**, which
points at Spring's `MessageSourceMessageInterpolator` (bridging Hibernate
Validator's interpolator to a Spring `MessageSource`) receiving a `null`
from `MessageSource.getMessage(code, args, null, locale)` and, somewhere in
the chain, concatenating/stringifying that `null` instead of taking the
"not found → keep original text" branch. Not traced further — could be
CratonVM's `MessageSource`/`ResourceBundle` no-match path returning
something other than the clean `null` HotSpot returns, or a
null-vs-non-null branch that resolves differently. `escapePrefix`/
`escapeSuffix` (same class, adjacent tests using literal `\{`/`\}`
sequences) both **pass**, narrowing this specifically to the "no message
found at all" path rather than escape-character handling in general.

## 8. `DefaultLogbackConfigurationTests.consoleLogCharsetShouldDefaultToUtf8WhenConsoleIsNull()`

```
expected: "UTF-8"
 but was: null
```

Full log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260723/shard3/logs/core_spring-boot.org.springframework.boot.logging.logback.DefaultLogbackConfigurationTests.out.log`

Uses `spy(new DefaultLogbackConfiguration(null))` +
`given(logbackConfiguration.getConsole()).willReturn(null)` (a Mockito spy
on the config object itself, stubbing only `getConsole()` — no `Console`
class mocking involved, so this is a different mechanism from the
`Console.ttyStatus()` native-registration gap documented separately in
`core-spring-boot-console-ttystatus-missing-native-20260723.md`). Not
determined this session whether this belongs with the "cross-method state
leakage" pattern in `core-spring-boot-crossmethod-state-leakage-residuals-20260723.md`
(its sibling `fileLogCharsetShouldUseSystemPropertyIfSet` failure in the
same class does) or is a separate, Mockito-spy-specific gap.

## Affected classes

| Module | Class |
|---|---|
| core/spring-boot | org.springframework.boot.system.ApplicationHomeTests |
| core/spring-boot | org.springframework.boot.SpringApplicationShutdownHookTests |
| core/spring-boot | org.springframework.boot.context.config.ConfigTreeConfigDataLocationResolverTests |
| core/spring-boot | org.springframework.boot.info.ProcessInfoTests |
| core/spring-boot | org.springframework.boot.logging.logback.LogbackRuntimeHintsTests |
| core/spring-boot | org.springframework.boot.logging.java.JavaLoggingSystemTests |
| core/spring-boot | org.springframework.boot.validation.MessageSourceMessageInterpolatorIntegrationTests |
| core/spring-boot | org.springframework.boot.logging.logback.DefaultLogbackConfigurationTests (1 of 3 failures: `consoleLogCharsetShouldDefaultToUtf8WhenConsoleIsNull`) |
