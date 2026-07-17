# `spring-boot-devtools` residual FAILs — 5 unrelated clusters

**Status: OPEN — found 2026-07-17**

Five `spring-boot-devtools` classes FAILed in the 2026-07-17 rerun for five
distinct, unrelated reasons (the other 5 devtools classes in this batch are
covered by the shared cross-module docs
`junit5-interceptingexecutableinvoker-layout-probe-livelock-cluster.md`
(3 HANGs) and `capturedoutput-empty-console-cluster.md` (2 FAILs)). All 5
confirmed CratonVM-specific (pass on the same-scope HotSpot baseline).

## Cluster 1 — `Logger.getLoggerContext()` returns null

| Class | Wall time | Tests |
|---|---:|---:|
| `RemoteUrlPropertyExtractorTests` | 7.6s | 5/5 fail |

All 5 tests fail with the identical NPE, always from the class's shared
`@AfterEach` hook, not from the test body itself:

```java
@AfterEach
void preventRunFailuresFromPollutingLoggerContext() {
	((Logger) LoggerFactory.getLogger(RemoteUrlPropertyExtractorTests.class)).getLoggerContext()
		.getTurboFilterList()
		.clear();
}
```
(`RemoteUrlPropertyExtractorTests.java:39-44`)

```
=> java.lang.NullPointerException: Cannot invoke "ch.qos.logback.classic.LoggerContext.getTurboFilterList()" because the return value of "ch.qos.logback.classic.Logger.getLoggerContext()" is null
   org.springframework.boot.devtools.RemoteUrlPropertyExtractorTests.preventRunFailuresFromPollutingLoggerContext(RemoteUrlPropertyExtractorTests.java:42)
```

Full log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard3/logs/module_spring-boot-devtools.org.springframework.boot.devtools.RemoteUrlPropertyExtractorTests.out.log`

**Root cause: not confirmed — hypothesis.** This is a `ch.qos.logback.classic.Logger` instance (obtained fresh via `LoggerFactory.getLogger(...)`, real bytecode) whose `loggerContext` field is null. This is adjacent to, but distinct from, the already-`FIXED` cluster
`docs/internal/springboot/logback-loggercontext-listenerlist-final-field-corruption-FIXED.md`, which was about `LoggerContext`'s own `loggerContextListenerList` field going null (traced to `LoggerContext.<init>()` being registered as a no-op in some paths — since fixed). Here the null field is on a different object (`Logger`, not `LoggerContext`) and a different field (`loggerContext`, set by `Logger`'s constructor when a logger is created under a context). Every real `Logger` should have this field populated at construction; a null value here is consistent with the *same class* of bug (a constructor-bypass/no-op-`<init>` registration) but for `ch.qos.logback.classic.Logger` specifically rather than `LoggerContext` — not verified against current native-registration tables this round.

## Cluster 2 — Derby embedded DB login timeout

| Class | Wall time | Tests |
|---|---:|---:|
| `DevToolsPooledDataSourceAutoConfigurationTests` | 94.1s | 1/11 fail |

```
JUnit Jupiter:DevToolsPooledDataSourceAutoConfigurationTests:inMemoryDerbyIsShutdown()
  => org.springframework.jdbc.CannotGetJdbcConnectionException: Failed to obtain JDBC Connection
     ...
   Caused by: java.sql.SQLTimeoutException: Login timeout exceeded.
     ...
     org.apache.derby.iapi.jdbc.InternalDriver.timeLogin(InternalDriver.java:341)
     ...
     com.zaxxer.hikari.pool.HikariPool.<init>(HikariPool.java:97)
```

Full log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard3/logs/module_spring-boot-devtools.org.springframework.boot.devtools.autoconfigure.DevToolsPooledData-10b5a1277bdd.out.log`

**Root cause: not confirmed — hypothesis, lower confidence than the other clusters here.** Only 1 of 11 tests in the class fails, and it's specifically the one opening a real embedded Derby connection through HikariCP with a login timeout. This has the shape of a slow-thread/scheduling issue (this is a shared, often loaded, host — see `feedback_shared_host_multitenant_confound` in project memory) rather than a deterministic CratonVM correctness bug; it may also be a genuine CratonVM-specific slowdown in thread scheduling/embedded-Derby's file I/O during the login handshake. Not root-caused this round — would need a repro run in isolation (no shard contention) to distinguish host load from a real CratonVM regression.

## Cluster 3 — `ChangeableUrls.fromClassLoader` returns empty instead of resolving a jar's `Class-Path`

| Class | Wall time | Tests |
|---|---:|---:|
| `ChangeableUrlsTests` | 0.3s | 1/6 fail |

```
JUnit Jupiter:ChangeableUrlsTests:urlsFromJarClassPathAreConsidered()
  => org.opentest4j.AssertionFailedError:
Expecting actual:
  []
to contain exactly (and in same order):
  [file:/.../project-core/target/classes/, file:/.../project-web/target/classes/, file:/.../project space/target/classes/, file:/.../f749591d-.../, file:/.../22ee3956-.../]
```

Full log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard3/logs/module_spring-boot-devtools.org.springframework.boot.devtools.restart.ChangeableUrlsTests.out.log`

The test builds a jar whose `META-INF/MANIFEST.MF` has a multi-value
`Class-Path` header (including a URL-encoded `project%20space/target/classes/`
entry), puts it on a `URLClassLoader`, and expects
`ChangeableUrls.fromClassLoader(...)` — which reads each jar URL's manifest
via `new JarFile(file).getManifest()` and its `Class-Path` main attribute —
to resolve every listed path. The actual result is **completely empty**,
not just missing the URL-encoded entry, meaning manifest `Class-Path`
resolution failed for the jar entirely, not just for the one tricky path.

**Root cause: not confirmed — hypothesis.** Same code path family
(`java.util.jar.Manifest`/`JarFile.getManifest()`) implicated in
`loader-tools-manifest-entries-and-zip-fidelity-residuals.md`'s Cluster 1
(signed-jar per-entry manifest attributes also silently missing after a
`JarFile`/`Manifest` round-trip) — worth cross-referencing if either gets a
confirmed root cause, but not merged into one doc here since the specific
manifest data lost differs (`Class-Path` main attribute vs. per-entry
digest sub-sections) and no common code site has been confirmed.

## Cluster 4 — `RestartClassLoader` resource/classloader identity residue

| Class | Wall time | Tests |
|---|---:|---:|
| `RestartClassLoaderTests` | 2.3s | 4/17 fail |

```
JUnit Jupiter:RestartClassLoaderTests:loadClassFromReloadableUrl()
  => expected: org.springframework.boot.devtools.restart.classloader.RestartClassLoader@16b90
      but was: jdk.internal.loader.ClassLoaders$AppClassLoader@c7

JUnit Jupiter:RestartClassLoaderTests:getResourcesFiltersDuplicates()
  => Expected size: 1 but was: 25 in: [...same sample.jar entry duplicated 25 times across many different junit-<pid> temp dirs from EARLIER tests in the same class...]
```

Full log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard3/logs/module_spring-boot-devtools.org.springframework.boot.devtools.restart.classloader.RestartClassLoaderTests.out.log`

Two related symptoms in the same class:
1. `Class.forName(name, false, this.reloadClassLoader)` returns a class
   whose `getClassLoader()` is the real JVM's `AppClassLoader`, not the
   custom `RestartClassLoader` instance that performed the load —
   `RestartClassLoader` was bypassed for class definition/identity.
2. `getResources(...)`/`getResourcesFiltersDuplicates()` return entries from
   **every prior test's temp jar** in the same process (25 entries across
   at least 12 distinct `junit-<timestamp>` temp directories), not just the
   current test's single `sample.jar` — i.e. resource lookups are not
   scoped to the current `RestartClassLoader` instance's own URL set, they
   appear to search/accumulate across all `RestartClassLoader` instances
   ever created in this process.

**Root cause: not confirmed — hypothesis.** The size-25-not-1 shape (each
prior test contributes exactly 2 duplicate entries, consistent with
`RestartClassLoader`'s own instance plus a parent/delegate) points at some
process-wide, unscoped cache or registry being consulted instead of the
specific `URLClassLoader`/`RestartClassLoader` instance's own classpath —
possibly the same family as the nested/URL classpath-resolution caches
found in `native-builtins/src/lang_class.rs`
(`plain_manifest_cache`/`nested_manifest_cache`, keyed by path string, used
for a different feature — `Class.getPackage()` manifest attributes — but
demonstrating this codebase does have process-wide caches in the
classloading area). Not confirmed against the actual code backing custom
`ClassLoader.getResources()`/`Class.forName(String,boolean,ClassLoader)`
this round.

## Cluster 5 — `NotSerializableException: unknown_4294967295`

| Class | Wall time | Tests |
|---|---:|---:|
| `HttpRestartServerTests` | 1.5s | 1/6 fail |

```
JUnit Jupiter:HttpRestartServerTests:sendBadSerializedData(CapturedOutput)
  => java.io.NotSerializableException: unknown_4294967295
     java.io.ObjectOutputStream.writeObject0(ObjectOutputStream.java:1085)
     java.io.ObjectOutputStream.writeObject(ObjectOutputStream.java:325)
     java.util.CollSer.writeObject(ImmutableCollections.java:1898)
     ...
     org.springframework.boot.devtools.restart.server.HttpRestartServerTests.serialize(HttpRestartServerTests.java:131)
     org.springframework.boot.devtools.restart.server.HttpRestartServerTests.sendBadSerializedData(HttpRestartServerTests.java:119)
```

Full log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard3/logs/module_spring-boot-devtools.org.springframework.boot.devtools.restart.server.HttpRestartServerTests.out.log`

**Root cause: grounded in source, not fully confirmed.**
`"unknown_4294967295"` = `"unknown_" + u32::MAX` — the exact sentinel
string produced by `Class.getName()`'s legacy fallback path in
`native-builtins/src/lang_class.rs:862-864`:

```rust
let name = ctx
    .class_name_of_id(class_id)
    .unwrap_or_else(|| format!("unknown_{}", class_id.as_u32()));
```

This fires when a `Class` mirror's `class_id` field doesn't resolve via
`class_name_of_id` — i.e. it's an invalid/unregistered `ClassId`
(`u32::MAX` looks like an all-bits-set sentinel, not a real allocated ID).
`NotSerializableException`'s message is normally the real class name
(`cl.getName()`) of whatever non-serializable object `ObjectOutputStream`
choked on; here that name computation itself hit the bogus-`ClassId`
fallback instead of the real class name. The call chain
(`CollSer.writeObject` — the serialization proxy for `List.of(...)`-family
immutable collections — recursing into `ObjectOutputStream.writeObject0`
for the list's elements) suggests the culprit `Class` mirror belongs to one
of the JDK's internal `java.util.ImmutableCollections` implementation
classes, which may not have a properly-registered `ClassId`/mirror in
CratonVM the way ordinary application classes do. Not confirmed which
specific class or why its `ClassId` is invalid — would need to add a debug
print at the `unwrap_or_else` call site or reproduce standalone with
`Class.forName("java.util.ImmutableCollections$ListN").getName()`-style
probes.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-devtools` | `org.springframework.boot.devtools.RemoteUrlPropertyExtractorTests` |
| `module/spring-boot-devtools` | `org.springframework.boot.devtools.autoconfigure.DevToolsPooledDataSourceAutoConfigurationTests` |
| `module/spring-boot-devtools` | `org.springframework.boot.devtools.restart.ChangeableUrlsTests` |
| `module/spring-boot-devtools` | `org.springframework.boot.devtools.restart.classloader.RestartClassLoaderTests` |
| `module/spring-boot-devtools` | `org.springframework.boot.devtools.restart.server.HttpRestartServerTests` |
