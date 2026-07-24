# Spring Boot core residual clusters (2026-07-23)

## Scope

This is the follow-up work after the focused `core/spring-boot` repair batch.
The repaired classes (`ApplicationPidFileWriterTests`, `BeanDefinitionLoaderTests`,
`ConfigDataEnvironmentPostProcessorIntegrationTests`,
`ConfigTreeConfigDataLocationResolverTests`) leave the clusters below. Each is
independent enough for a separate worktree.

Do not merge a timeout-only change: collect the class stderr log and a VM stack
sample before changing JIT admission or a native-method bridge.

## Reproduction harness

Run from the isolated CratonVM worktree. Use a unique executable name for each
build and replace `<class-list.tsv>` with a TSV having the header
`module<TAB>class` and the selected class list.

```powershell
$env:JAVA_HOME = 'C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot'
$exe = '<worktree>\target\release\cratonvm-springboot-<cluster>-<date>.exe'
powershell.exe -NoProfile -ExecutionPolicy Bypass -File apps\spring-boot-suite-runner\run-spring-boot-suite.ps1 `
  -SpringBootRoot 'C:\craton\CratonVM\apps\spring-boot' `
  -Vm craton -Exe $exe -JdkHome $env:JAVA_HOME -ClassList <class-list.tsv> `
  -RunName core39-<cluster>-jit-<date> -Parallel 2 -TimeoutSec 300

# Repeat with -VmArgs '--nojit' and a distinct RunName.
```

Before closure, rerun the selected list in both modes and then rerun
`apps/spring-boot-suite-runner/core-spring-boot-residual39-20260723.tsv`.

## Cluster A — property/configuration and origin loading

Likely paths: `Properties`/map backing, property-source enumeration,
configuration metadata, origin tracking, and AOT reflection.

```text
core/spring-boot	org.springframework.boot.context.properties.ConfigurationPropertiesBeanRegistrationAotProcessorTests
core/spring-boot	org.springframework.boot.context.properties.source.ConfigurationPropertySourcesTests
core/spring-boot	org.springframework.boot.context.properties.bind.MapBinderTests
core/spring-boot	org.springframework.boot.env.OriginTrackedPropertiesLoaderTests
core/spring-boot	org.springframework.boot.env.OriginTrackedYamlLoaderTests
```

The first two timed out in the JIT diagnostic; `OriginTrackedPropertiesLoaderTests`
failed one assertion. `MapBinderTests` now passes JIT (45/45) after the
`Properties.computeIfAbsent` bridge, but its no-JIT run still has two
placeholder-expansion assertions. Sample the timeouts before treating them as
one root cause.

## Cluster B — diagnostics, process metadata, and byte/URL utilities

Likely paths: error construction/stack traces, process/environment metadata,
Base64 protocol handling, and mutable byte buffers.

```text
core/spring-boot	org.springframework.boot.diagnostics.analyzer.NoSuchMethodFailureAnalyzerTests
core/spring-boot	org.springframework.boot.info.ProcessInfoTests
core/spring-boot	org.springframework.boot.io.Base64ProtocolResolverTests
core/spring-boot	org.springframework.boot.json.AppendableByteArrayTests
```

All four failed in the JIT diagnostic. Keep execution together, but split fixes
if their VM paths diverge.

## Cluster C — logging bootstrap and backend contracts

Likely paths: JUL, Log4j2, Logback, resource discovery, and parallel logging
initialization. `LoggingApplicationListenerTests` timed out in the diagnostic;
sample it before changing logging stubs.

```text
core/spring-boot	org.springframework.boot.context.logging.LoggingApplicationListenerTests
core/spring-boot	org.springframework.boot.logging.java.JavaLoggingSystemTests
core/spring-boot	org.springframework.boot.logging.log4j2.Log4j2LoggingSystemPropertiesTests
core/spring-boot	org.springframework.boot.logging.log4j2.Log4J2LoggingSystemTests
core/spring-boot	org.springframework.boot.logging.log4j2.SpringBootPropertySourceTests
core/spring-boot	org.springframework.boot.logging.log4j2.SpringProfileArbiterTests
core/spring-boot	org.springframework.boot.logging.logback.DefaultLogbackConfigurationTests
core/spring-boot	org.springframework.boot.logging.logback.LogbackConfigurationAotContributionTests
core/spring-boot	org.springframework.boot.logging.logback.LogbackLoggingSystemParallelInitializationTests
core/spring-boot	org.springframework.boot.logging.logback.LogbackLoggingSystemTests
core/spring-boot	org.springframework.boot.logging.logback.LogbackRuntimeHintsTests
core/spring-boot	org.springframework.boot.logging.logback.SpringBootJoranConfiguratorTests
core/spring-boot	org.springframework.boot.logging.LogbackAndLog4J2ExcludedLoggingSystemTests
core/spring-boot	org.springframework.boot.logging.LoggingSystemTests
```

`JavaLoggingSystemTests` has a HotSpot baseline discrepancy in this Windows
fixture. Establish the current HotSpot result before calling an assertion a VM defect.

## Cluster D — application lifecycle, SSL, and validation — FIXED (9 root causes), 1 residual OPEN

Likely paths: launch/shutdown hooks, filesystem/process discovery, JKS/TLS,
message interpolation, and servlet registration.

**Status: 9 root causes fixed** (URLClassLoader `%20` decode +
per-instance namespace, JCA no-such-provider ordering, Base64 error
wording, `ResourceBundle.getObject` missing-key contract,
`Thread.getState()` TIMED_WAITING, `getTextBanner` S111r25 stub restored to
a real capture-aware lookup, `ConfigurationClassEnhancer`'s "no visible
constructors" guard + `BeanDefinitionStoreException` wrapping,
`Throwable.printStackTrace(System.out/err)` bypassing a redirected/tee'd
stream) — 9/10 classes now fully clean, including `SpringApplicationTests`
(was 98/104, now 104/104). Full writeup:
`docs/internal/fixed-suite-bugs/springboot/core39-clusterD-lifecycle-ssl-validation-FIXED.md`.
**1 residual OPEN** (tracked in that doc): `SpringApplicationNoWebTests`
fails under JIT only (passes under `--nojit`) — a genuine cross-package
JIT-to-JIT call/dispatch bug between `org/codehaus/groovy/reflection` and
`org/codehaus/groovy/util` (empirically bisected, not yet root-caused to a
specific instruction — see the FIXED doc for the full bisection trail).

```text
core/spring-boot	org.springframework.boot.SimpleMainTests
core/spring-boot	org.springframework.boot.SpringApplicationNoWebTests
core/spring-boot	org.springframework.boot.SpringApplicationShutdownHookTests
core/spring-boot	org.springframework.boot.SpringApplicationTests
core/spring-boot	org.springframework.boot.ssl.jks.JksSslStoreBundleTests
core/spring-boot	org.springframework.boot.system.ApplicationHomeTests
core/spring-boot	org.springframework.boot.system.ApplicationPidTests
core/spring-boot	org.springframework.boot.validation.MessageInterpolatorFactoryWithoutElIntegrationTests
core/spring-boot	org.springframework.boot.validation.MessageSourceMessageInterpolatorIntegrationTests
core/spring-boot	org.springframework.boot.web.servlet.NoSpringWebFilterRegistrationBeanTests
```

## Green controls

Keep these in the full-manifest validation because they isolate this batch's
repairs:

```text
core/spring-boot	org.springframework.boot.BeanDefinitionLoaderTests
core/spring-boot	org.springframework.boot.context.ApplicationPidFileWriterTests
core/spring-boot	org.springframework.boot.context.config.ConfigDataEnvironmentPostProcessorIntegrationTests
core/spring-boot	org.springframework.boot.context.config.ConfigTreeConfigDataLocationResolverTests
core/spring-boot	org.springframework.boot.diagnostics.analyzer.JakartaApiValidationExceptionFailureAnalyzerTests
core/spring-boot	org.springframework.boot.env.NoSnakeYamlPropertySourceLoaderTests
```
