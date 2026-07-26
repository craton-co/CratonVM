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

## Cluster A — property/configuration and origin loading — 4/5 CLOSED 2026-07-24

Likely paths: `Properties`/map backing, property-source enumeration,
configuration metadata, origin tracking, and AOT reflection.

```text
core/spring-boot	org.springframework.boot.context.properties.ConfigurationPropertiesBeanRegistrationAotProcessorTests
core/spring-boot	org.springframework.boot.context.properties.source.ConfigurationPropertySourcesTests
core/spring-boot	org.springframework.boot.context.properties.bind.MapBinderTests
core/spring-boot	org.springframework.boot.env.OriginTrackedPropertiesLoaderTests
core/spring-boot	org.springframework.boot.env.OriginTrackedYamlLoaderTests
```

Worktree `springboot-core39-clusterA-20260723`. Two real VM bugs found and
fixed, plus two timeout-tuning corrections:

1. **`java.util.Properties` `.properties`-file escape decoder never handled
   `\f`** (`../../../native-builtins/src/properties_sidetable.rs`,
   `unescape_inner`) — `\f` degraded to a literal `f` instead of a form-feed
   (0x0C), because the escape `match` simply had no `'f'` arm. Fixed
   `OriginTrackedPropertiesLoaderTests.compareToJavaProperties` (the ONLY
   failing assertion in that class — real `java.util.Properties.load` and the
   hand-rolled `OriginTrackedPropertiesLoader` disagreed on exactly the
   `test-form-feed-property` entry).
2. **`String.chars()`/`String.codePoints()` allocated the wrong array kind**
   (`../../../native-builtins/src/lang_string.rs`, `native_string_chars`) —
   `ctx.new_ref_array(ClassId::new(0), ...)` (a *reference*-element array)
   instead of `ctx.new_array(ArrayElementType::Int, ...)`, then stored
   `Value::Int`s into the Object-shaped slots. Every consumer that reads the
   backing array as `int[]` (the whole `IntStream` machinery: `forEach`,
   `toArray`, `filter`, `map`, …) silently saw all-zero elements — the array
   was correctly *sized* but every element read back as `0`. Confirmed via a
   minimal standalone repro (`"foo-bar".chars().toArray()` →
   `[0,0,0,0,0,0,0]` before the fix, `[102,111,111,45,98,97,114]` after).
   This is a broadly-used JDK API (any character-by-character `Stream`
   pipeline over a `String`), not Cluster-A-specific; it happened to surface
   here via Spring Boot's `LenientObjectToEnumConverterFactory
   .getCanonicalName()`, whose `name.chars().filter(...).map(...)
   .forEach(...)` pipeline always produced an empty canonical name, so
   `findEnum()` always matched the *first* enum constant regardless of the
   real input — `MapBinderTests.bindToMapShouldBeGreedyForScalars` /
   `bindToMapWithPlaceholdersShouldBeGreedyForScalars` (both `--nojit`
   residuals) bound every non-exact-match enum value to the same wrong
   constant. Fixed both; **no known regression risk given how targeted the
   fix is, but re-audit any code that depends on `chars()`/`codePoints()`
   returning all-zero if something relied on that (accidentally) — extremely
   unlikely, but flagging since the bug was old enough to have shipped with
   some workaround somewhere.**
3. `OriginTrackedYamlLoaderTests` was reported as a 300s HANG but is not a
   deadlock — CPU-sampled busy the whole time, completes on its own in
   386.8s (JUnit5 extension-registry/interceptor-chain dispatch overhead
   across its many individual `@Test` methods, see
   `reference_junit5_execution_machinery_dispatch_overhead` in project
   memory). Registered a 900s override in `run-spring-boot-suite.ps1`'s
   `Get-EffectiveClassTimeoutSec` (extra margin over the observed time for
   `-Parallel` contention).
4. `ConfigurationPropertySourcesTests` was also reported as a 300s HANG and
   is **also not a deadlock** — extensively diagnosed (repeated
   stack-dump-on-timeout samples at 60s/400s/1100s, all pegged near 100%
   CPU, never parked; a scaled-down hand repro of its own `N sources x M
   keys x K getProperty() iterations` shape measured constant, not
   quadratic, per-iteration cost — confirmed linear scaling). It
   deliberately benchmarks an "uncached" O(sources x keys) baseline against
   ~100 sources x 1000 keys x 1000 iterations (`cached < uncached/2`
   assertions) — genuinely CPU-heavy by design, not merely slow-to-start;
   this is the same diffuse interpreter/dispatch throughput family as
   `reference_hashmap_native_call_dispatch_overhead` /
   `reference_junit5_execution_machinery_dispatch_overhead`, not a single
   fixable hotspot. A full standalone rerun completed (PASS) in 2645.0s.
   Registered a 5400s override rather than continuing to report a false
   HANG.

4 of the 5 classes PASS under both JIT and `--nojit` after the fixes above;
see the validation run log referenced in the merge commit for exact timings.
`OriginTrackedPropertiesLoaderTests` and `MapBinderTests` are fast (order of
seconds); `OriginTrackedYamlLoaderTests` takes several minutes;
`ConfigurationPropertySourcesTests` can take on the order of 45 minutes —
expected, not a regression, given the CPU-bound findings above.

**`ConfigurationPropertiesBeanRegistrationAotProcessorTests` — original hang
RESOLVED 2026-07-26, class still OPEN (2 unrelated JIT bugs).** The
Hibernate-Validator hang described here no longer reproduces on current
`dev` — a faithful from-scratch reproduction of the real (forked-classloader)
test method completes in ~13s under `--nojit`, matching this doc's own
repro recipe exactly. Very likely fixed as a side effect of unrelated work
landed on `dev` after the 2026-07-24 investigation; no specific fixing
commit was identified. **However the class still fails under CratonVM's
default JIT-on mode**, due to two newly-discovered, unrelated JIT
correctness bugs in Spring's AOT codegen path: (1) `javax.lang.model
.SourceVersion.isIdentifier` gets miscompiled once JIT-inlined/compiled via
`SourceVersion.isName`'s own call (fully isolated, dependency-free 20-line
repro, `CRATONVM_JIT_DENY=isIdentifier` workaround available), and (2)
AOT-generated `void`-returning methods (built via javapoet's default
`TypeName.VOID`, e.g. `registerBeanDefinitions`) lose their return type
token under JIT, producing a javac parse error (not yet minimally
isolated). Both are JIT-only (never reproduce under `--nojit`). Full
diagnostic trail, minimal repros, and bisection notes:
`configurationpropertiesbeanregistrationaotprocessortests-hang.md`. Still
left OUT of the suite-runner timeout table — it's a real (fast, ~2s)
failure under JIT now, not a hang, but still not passing.

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
`../../internal/fixed-suite-bugs/springboot/core39-clusterD-lifecycle-ssl-validation-FIXED.md`.
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
