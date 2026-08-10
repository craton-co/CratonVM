# `LoggingSystemTests` + `LogbackAndLog4J2ExcludedLoggingSystemTests` — SLF4J `ServiceConfigurationError` under `@ClassPathExclusions`, likely `ServiceLoader`'s unconditional flat-classpath scan

**Status: OPEN — new finding 2026-08-10.** Collector-agnostic: reproduces
identically under Generational, G1, and ZGC.

**Likely the same bug family as
[`jakartaapivalidationexceptionfailureanalyzertests-classpath-exclusion-leak-20260810.md`](jakartaapivalidationexceptionfailureanalyzertests-classpath-exclusion-leak-20260810.md),
filed the same round from the same rerun.** Both describe a correctly
loader-scoped resource lookup unconditionally supplemented/overridden by a
flat, process-wide classpath scan that ignores a `ModifiedClassPathClassLoader`'s
exclusions — this doc's `service_loader.rs::discover_providers` and that
doc's `classloader.rs::ucl_find_resources` are different call sites, same
shape. See that doc's note on the `classloader.rs` "WF32-fix" precedent for
this exact anti-pattern in a third, already-fixed resource. Worth
investigating together rather than independently before landing a fix.

## Symptom

Reconciling the 139-class union of non-passed classes from the three
2026-08-08f full-suite reruns (`craton-nonpassed-{default,g1,zgc}-20260808f`)
on `dev@6365de194`, two `core/spring-boot` classes fail with the same
underlying exception:

| Class | Test | GC | Shard | Result | Log |
|---|---|---|---|---|---|
| `org.springframework.boot.logging.LoggingSystemTests` | `julIsUsedInTheAbsenceOfLogbackAndLog4j2()` | default | s1 | FAIL 4.925s, tests=8 failed=1 | `craton-nonpassed-default-20260808f-s1/all-jit/logs/core_spring-boot.org.springframework.boot.logging.LoggingSystemTests.{out,err}.log` |
| | | G1 | s1 | FAIL 4.719s, tests=8 failed=1 | `craton-nonpassed-g1-20260808f-s1/all-jit/logs/...LoggingSystemTests.{out,err}.log` |
| | | ZGC | s1 | FAIL 6.370s, tests=8 failed=1 | `craton-nonpassed-zgc-20260808f-s1/all-jit/logs/...LoggingSystemTests.{out,err}.log` |
| `org.springframework.boot.logging.LogbackAndLog4J2ExcludedLoggingSystemTests` | `whenLogbackAndLog4J2AreNotPresentJULIsTheLoggingSystem()` | default | s1 | FAIL 1.431s, tests=1 failed=1 | `craton-nonpassed-default-20260808f-s1/all-jit/logs/core_spring-boot.org.springframework.boot.logging.LogbackAndLog4J2Excluded-634cc7399fc7.{out,err}.log` |
| | | G1 | s1 | FAIL 1.451s, tests=1 failed=1 | `craton-nonpassed-g1-20260808f-s1/all-jit/logs/...LogbackAndLog4J2ExcludedLoggingSystemTests.{out,err}.log` |
| | | ZGC | s1 | FAIL 2.038s, tests=1 failed=1 | `craton-nonpassed-zgc-20260808f-s1/all-jit/logs/...LogbackAndLog4J2Excluded-634cc7399fc7.{out,err}.log` |

(all under `apps/spring-boot-suite-runner/.suite/results/`). Both fail with
the byte-identical exception, in all three GC runs:

```
=> java.util.ServiceConfigurationError: org.slf4j.spi.SLF4JServiceProvider: Provider ch.qos.logback.classic.spi.LogbackServiceProvider not found
   java.util.ServiceConfigurationError.<init>(ServiceConfigurationError.java:52)
   org.slf4j.LoggerFactory.findServiceProviders(LoggerFactory.java:132)
   org.slf4j.LoggerFactory.bind(LoggerFactory.java:195)
   org.slf4j.LoggerFactory.performInitialization(LoggerFactory.java:187)
   org.slf4j.LoggerFactory.getProvider(LoggerFactory.java:511)
   org.slf4j.MarkerFactory.<clinit>(MarkerFactory.java:53)
   org.apache.commons.logging.impl.Slf4jLogFactory.<clinit>(Slf4jLogFactory.java:255)
   org.apache.commons.logging.LogFactory.newStandardFactory(LogFactory.java:1432)
   org.apache.commons.logging.LogFactory.getFactory(LogFactory.java:872)
   org.apache.commons.logging.LogFactory.getLog(LogFactory.java:921)
   org.springframework.core.io.support.SpringFactoriesLoader.<clinit>(SpringFactoriesLoader.java:102)
   org.springframework.boot.logging.LoggingSystemFactory.lambda$fromSpringFactories$0(LoggingSystemFactory.java:46)
   org.springframework.boot.logging.DelegatingLoggingSystemFactory.getLoggingSystem(DelegatingLoggingSystemFactory.java:43)
   org.springframework.boot.logging.LoggingSystem.get(LoggingSystem.java:170)
   ...LoggingSystemTests.julIsUsedInTheAbsenceOfLogbackAndLog4j2(LoggingSystemTests.java:58)   [or LogbackAndLog4J2ExcludedLoggingSystemTests.java:36]
```

Both test methods carry `@ClassPathExclusions({ "logback-*.jar", "log4j-*.jar" })`
(`LoggingSystemTests.java:56`; `LogbackAndLog4J2ExcludedLoggingSystemTests`
carries the class-level equivalent — same annotation, same exclusion set)
and assert `LoggingSystem.get(...)` falls back to `JavaLoggingSystem` when
neither logging backend is on the classpath. Both go through JUnit's
`ModifiedClassPathExtension`, which builds an isolated
`ModifiedClassPathClassLoader` (a `URLClassLoader` subclass) whose URL array
omits the excluded jars, and runs the annotated method inside a fresh nested
`Launcher` execution under that loader.

Neither test's own assertion is ever reached — the failure happens earlier,
during `SpringFactoriesLoader.<clinit>`'s first touch of commons-logging,
which (via `jcl-over-slf4j`) routes through SLF4J's `LoggerFactory.bind()`,
which enumerates `ServiceLoader.load(SLF4JServiceProvider.class)`. The
`ServiceConfigurationError` message shape — "`<service>: Provider
<providerClassName> not found`" — is SLF4J's own `ServiceConfigurationError`
constructor firing when the underlying `ServiceLoader` iterator finds a
provider-configuration-file entry naming a class, but that class cannot
actually be loaded through the given `ClassLoader`. That only happens when a
`META-INF/services/org.slf4j.spi.SLF4JServiceProvider` file naming
`ch.qos.logback.classic.spi.LogbackServiceProvider` is visible to
`ServiceLoader`, while the class itself is not — which is exactly what
excluding `logback-*.jar` from this loader should make impossible, since
both the services file and the provider class live inside the same jar. On
real HotSpot (per the existing `!springboot-ldap-dsa-tls-windows-only-gap.md`
and `filewatchertests-windows-symlink-privilege-gap-20260807.md` docs'
precedent of comparing against stock JDK 25 on this same host) `ServiceLoader`
would see neither the entry nor the class, `providersList` would end up
empty, and `LoggingSystem.get()` would fall through to `JavaLoggingSystem`
without error — which is what both tests assert.

## Checked against existing docs first

No existing doc names either class. The one doc that looked adjacent —
`log4j2-logback-loggingsystemtests-modifiedclasspath-throughput-hang-20260807.md`
— covers a **different pair of classes** (`Log4J2LoggingSystemTests`,
`LogbackLoggingSystemTests`) and a **different failure mode** (300s HANG
from `ModifiedClassPathExtension`'s per-method nested-`Launcher` cost, not an
exception). Confirmed via `grep -rli` over `docs/known-issues/` and
`docs/internal/fixed-suite-bugs/` for both exact class names and for
`ServiceConfigurationError`/`LogbackServiceProvider`/`SLF4JServiceProvider`
— no hits naming this exception against either of these two classes. This is
a new finding, not a drifted or reconfirmed instance of a prior one.

## Root cause — candidate mechanism, not fully confirmed

`native-builtins/src/service_loader.rs`'s `discover_providers` (the function
backing `ServiceLoader`'s provider list) does two passes:

1. **Loader-scoped resolution** (lines 862-1193): when the `ServiceLoader`
   carries a non-builtin `ClassLoader` (`ModifiedClassPathClassLoader`
   qualifies — `is_builtin_loader_class` at `classloader.rs:1863` only
   matches the JDK's own `java/net/URLClassLoader` etc. by exact name, not
   subclasses), it calls `loader.getResources(resource)` virtually
   (`service_loader.rs:950-954`) and reads bytes from the returned URLs.
   `classloader.rs`'s `getResources` native has explicit handling to keep a
   `URLClassLoader`-family loader's resolution scoped to its own URL array
   rather than falling into a flat scan (`classloader.rs:5250-5351`, see
   also the WF32-fix history at the top of that file for the same class of
   bug in a different resource). This path should correctly return **zero**
   providers here, since the loader's own URL array excludes
   `logback-classic.jar`.
2. **Flat classpath scan** (`service_loader.rs:1196-1207`), which runs
   **unconditionally** after step 1 (gated only on `loader_is_jboss_module`,
   which is false here):

   ```rust
   // Flat classpath scan: providers listed directly at
   // META-INF/services/<svc> on the classpath (normal case for
   // non-embedded loaders and JDK built-in providers).
   let descriptors = if loader_is_jboss_module {
       Vec::new()
   } else {
       ctx.find_all_resource_bytes(&resource)
   };
   for bytes in &descriptors {
       parse_provider_lines(bytes, &mut providers);
   }
   ```

   `ctx.find_all_resource_bytes` is the same process-wide, all-jars flat
   scan the WF32-fix comment in `classloader.rs` describes for
   `MANIFEST.MF` — it has no notion of the specific `ClassLoader`'s
   exclusions. If `logback-classic.jar` is present anywhere on the base VM
   classpath (plausible: it's a genuine dependency of the `core/spring-boot`
   module, needed by the many *other*, unannotated logging tests in the same
   module, e.g. `LogbackLoggingSystemTests`), this scan re-adds
   `ch.qos.logback.classic.spi.LogbackServiceProvider` to `providers`
   regardless of what step 1 found or what this specific loader excludes.
   `providers` is then handed to the `ServiceLoader` iteration machinery,
   which tries to load each name through the (correctly restrictive)
   `ModifiedClassPathClassLoader` — and fails on this one, producing exactly
   the observed `ServiceConfigurationError`.

**Open question this doc does not resolve:** `LoggingSystemTests` has a
sibling test on the same class, `log4j2IsUsedInTheAbsenceOfLogback()`
(`LoggingSystemTests.java:50-53`, excludes only `logback-*.jar`, not
`log4j-*.jar`), which **passes** in this same run (`LoggingSystemTests`
shows `tests=8 failed=1`, i.e. only `julIsUsedInTheAbsenceOfLogbackAndLog4j2`
— which excludes *both* `logback-*.jar` and `log4j-*.jar` — fails). Both
tests should equally touch `SpringFactoriesLoader.<clinit>` and thus SLF4J's
provider binding on their very first line, so if the flat-scan mechanism
above is the whole story, both should be equally exposed to a re-added
`LogbackServiceProvider` entry. Why only the both-excluded test fails is not
established here — possibilities include some interaction between which
`log4j` provider is *also* found via the correctly-scoped loader path in the
log4j2-present case masking the issue, or some other asymmetry in what
`find_all_resource_bytes` returns that hasn't been traced. Flagging this
explicitly rather than asserting full confirmation.

## What would confirm this

- Rerun either failing test standalone with `CRATONVM_DIAG_SERVICELOADER=1`
  (`types/src/flags.rs:1841`; gates `nbflags().diag_serviceloader`, which
  drives the `[SL-LOADER-DBG]` tracing throughout `service_loader.rs`,
  including the `getResources` call and its result at
  `service_loader.rs:956-964`) to see
  directly whether step 1's loader-scoped `getResources` genuinely returns
  empty, and whether `providers` is empty before the flat scan at line 1199
  and non-empty (containing `LogbackServiceProvider`) after it.
- Run the same trace against the passing `log4j2IsUsedInTheAbsenceOfLogback`
  to resolve the open question above — does the flat scan also inject
  `LogbackServiceProvider` there, and if so, why doesn't SLF4J's provider
  binding choke on it the same way?
- If confirmed, the fix shape is narrow: skip the flat-classpath scan
  (`service_loader.rs:1196-1207`) whenever step 1 already ran against a
  non-builtin, non-jboss loader (`loader_ref_opt.is_some() &&
  !loader_is_jboss_module`), trusting that loader-scoped `getResources`
  result — including when it is legitimately empty — rather than
  supplementing it with an unscoped global scan. This is the same fix shape
  the WF32-fix history already applied for `MANIFEST.MF` enumeration
  under flat-classpath flooding, generalized to jar-exclusion correctness
  rather than just an enumeration-count cap.

## Affected classes (this run)

- `core/spring-boot` — `org.springframework.boot.logging.LoggingSystemTests`
  (1/8 fail: `julIsUsedInTheAbsenceOfLogbackAndLog4j2`)
- `core/spring-boot` — `org.springframework.boot.logging.LogbackAndLog4J2ExcludedLoggingSystemTests`
  (1/1 fail: `whenLogbackAndLog4J2AreNotPresentJULIsTheLoggingSystem`)

## Related

- `log4j2-logback-loggingsystemtests-modifiedclasspath-throughput-hang-20260807.md`
  — different classes, different failure mode (HANG, not exception), same
  `ModifiedClassPathExtension`/`@ClassPathExclusions` machinery.
- `configtree-applicationtemp-windows-symlink-privilege-RETIRED-20260809.md`
  — an unrelated symlink-privilege gap, but a useful precedent for "the bare
  `=>` summary line carries no exception message, don't match on message
  text."
