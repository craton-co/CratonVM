# `spring-boot-jdbc` — a subclass's `loadClass(String,boolean)` override is not honored by `ClassUtils.isPresent`/`Class.forName`, so "hidden" driver/pool classes are still found

**Status: OPEN — found 2026-07-17**

## Symptom

Three tests across two `spring-boot-jdbc` classes construct a small
`URLClassLoader` subclass that overrides the protected
`loadClass(String name, boolean resolve)` method to throw
`ClassNotFoundException` for specific class-name prefixes (simulating "this
dependency is absent from the classpath"), then pass that loader to
`DataSourceBuilder.create(classLoader)` or
`DataSourceProperties.setBeanClassLoader(classLoader)` /
`ApplicationContextRunner.withClassLoader(classLoader)`. On CratonVM the
override never takes effect for these call paths — the "hidden" class is
still found and used, or (for the "no embedded database available" case) no
exception is raised at all.

| Class | Test(s) | Expected | Actual |
|---|---|---|---|
| `org.springframework.boot.jdbc.DataSourceBuilderTests` | `buildWhenHikariNotAvailableReturnsTomcatDataSource`, `buildWhenTomcatDataSourceWithNullPasswordReturnsDataSource`, `buildWhenHikariAndTomcatNotAvailableReturnsDbcp2DataSource`, `buildWhenHikariAndTomcatAndDbcpNotAvailableReturnsOracleUcpDataSource`, `buildWhenDbcp2DataSourceWithNullPasswordReturnsDbcp2DataSource` (5 failures / 40 tests) | Tomcat/DBCP2/UCP `DataSource` (Hikari "hidden" via `HidePackagesClassLoader("com.zaxxer.hikari")`, etc.) | `com.zaxxer.hikari.HikariDataSource` every time — the "hidden" package was still resolved |
| `org.springframework.boot.jdbc.autoconfigure.DataSourcePropertiesTests` | `determineUrlWithNoEmbeddedSupport` (1 failure / 16 tests) | `DataSourceBeanCreationException` ("Failed to determine suitable jdbc url") when `org.h2`/`org.apache.derby`/`org.hsqldb` are hidden via `FilteredClassLoader` | No exception raised — an embedded URL was determined anyway |
| `org.springframework.boot.jdbc.autoconfigure.DataSourceAutoConfigurationTests` | `testBadUrl` (1 of 2 failures / 24 tests) | Context fails to start (`BeanCreationException`) — bad JDBC URL with all embedded-DB drivers hidden via `DisableEmbeddedDatabaseClassLoader` | Context started successfully |

Representative trace (`DataSourceBuilderTests`):

```
java.lang.AssertionError:
Expecting actual:
  HikariDataSource (null)
to be an instance of:
  org.apache.tomcat.jdbc.pool.DataSource
but was instance of:
  com.zaxxer.hikari.HikariDataSource
       org.springframework.boot.jdbc.DataSourceBuilderTests.buildWhenHikariNotAvailableReturnsTomcatDataSource(DataSourceBuilderTests.java:99)
```

Full logs:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard3/logs/module_spring-boot-jdbc.org.springframework.boot.jdbc.DataSourceBuilderTests.out.log`,
`.../module_spring-boot-jdbc.org.springframework.boot.jdbc.autoconfigure.DataSourcePropertiesTests.out.log`,
`.../module_spring-boot-jdbc.org.springframework.boot.jdbc.autoconfigure.DataSourceAutoConfigurationTests.out.log`

All three hider classes share the exact same shape (verified in source):

```java
// DataSourceBuilderTests$HidePackagesClassLoader (uses a stream + method reference)
protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
    if (Arrays.stream(this.hiddenPackages).anyMatch(name::startsWith)) {
        throw new ClassNotFoundException();
    }
    return super.loadClass(name, resolve);
}

// org.springframework.boot.test.context.FilteredClassLoader / DataSourceAutoConfigurationTests$DisableEmbeddedDatabaseClassLoader
// (a plain for-loop, no lambda/method-reference at all)
protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
    for (...) { if (<name matches>) throw new ClassNotFoundException(); }
    return super.loadClass(name, resolve);
}
```

Both a method-reference/stream-based predicate (`HidePackagesClassLoader`)
and a plain `for` loop with direct `String.startsWith` calls
(`FilteredClassLoader.PackageFilter`, `DisableEmbeddedDatabaseClassLoader`)
show the identical symptom, which rules out a lambda/method-reference
dispatch bug as the cause (that would only explain the first shape) — the
common factor is purely "a `URLClassLoader` subclass overrides
`loadClass(String,boolean)` and is reached via `ClassUtils.isPresent`/
`Class.forName(name, false, loader)`".

**Note:** a sibling test in the same `DataSourceAutoConfigurationTests`
class, `dataSourceWhenNoConnectionPoolsAreAvailableWithUrlDoesNotCreateDataSource`
(uses the same-style `hideConnectionPools()` `FilteredClassLoader` helper),
**passes**. That test's assertion is driven by Spring's `@ConditionalOnClass`
condition evaluation (ASM-based bytecode/annotation scanning over the
configured classloader), not by a runtime `ClassUtils.isPresent`/
`Class.forName` call — consistent with the hypothesis below: the gap is
specific to the `Class.forName(name, false, loader)` / `ClassLoader.loadClass`
native path, not classloading in general.

## Root cause (hypothesis, not fully confirmed)

`DataSourceBuilder`'s `ClassUtils.isPresent(className, classLoader)` /
`DataSourceProperties`'s embedded-database detection ultimately call
`Class.forName(name, false, classLoader)`. That native
(`native-builtins/src/lang_class.rs`, the 3-arg `forName` handler around
line 1690) correctly routes through `ctx.invoke_virtual(loader, "loadClass",
"(Ljava/lang/String;)Ljava/lang/Class;", ...)` (see the `RKC16N.12` comment
at `lang_class.rs:1708-1730`).

That one-arg `loadClass(String)` call is itself forced onto a CratonVM
**native** implementation instead of real `java/lang/ClassLoader` bytecode
— `vm/src/runtime/interpreter.rs:23977-23984` (`force_native_over_real_jdk_bytecode`)
unconditionally forces the native path whenever dispatch resolves to
`java/lang/ClassLoader.loadClass(String)` or `loadClass(String,Z)` declared
directly on the base `ClassLoader` class (i.e. the receiver's own class does
not itself declare `loadClass(String)`, only the base). The native handler
(`native-builtins/src/classloader.rs::cl_load_class`) then calls
`receiver_overrides_load_class_resolve(ctx, this)` — a superclass-chain walk
that looks for a **declared** `loadClass(String,boolean)` override — and, if
found, dispatches virtually into the receiver's own 2-arg override
(`classloader.rs:1376-1383`).

Reading that routing logic in isolation, it looks correct for exactly this
scenario: `HidePackagesClassLoader`/`FilteredClassLoader`/
`DisableEmbeddedDatabaseClassLoader` all directly `extends URLClassLoader`
and directly declare `loadClass(String,boolean)`, so
`receiver_overrides_load_class_resolve` should find the override on the
first loop iteration and correctly reroute. **This was verified by reading
the routing code, not by a live repro** (this triage pass does not build or
run the VM) — so either (a) there is a subtler bug in that routing that a
source read did not surface (e.g. a caching/memoization issue reusing a
resolution keyed only on the declaring class rather than the concrete
receiver — see the sibling `reference_invoke_virtual_native_dispatch_cache_quirk`
pattern already documented for other native-to-Java callbacks), or (b) the
actual break is elsewhere in the call chain between Spring's
`ClassUtils.isPresent`/`resolveClassName` and this native (not read in this
pass).

**What would confirm/refute:** a standalone repro — construct a
`URLClassLoader` subclass overriding `loadClass(String,boolean)` to always
throw `ClassNotFoundException`, then call
`Class.forName("java.lang.String", false, thatLoader)` — should throw. If it
throws in isolation, the bug is further up the Spring `ClassUtils` call
chain (not the native `loadClass` routing); if it does NOT throw, the bug is
confirmed inside `cl_load_class`/`receiver_overrides_load_class_resolve`
despite reading correct on paper, most likely a dispatch-cache issue as in
the sibling `reference_invoke_virtual_native_dispatch_cache_quirk`.

## Affected classes

| Module | Class | Failing tests |
|---|---|---|
| `module/spring-boot-jdbc` | `org.springframework.boot.jdbc.DataSourceBuilderTests` | 5 of 40 |
| `module/spring-boot-jdbc` | `org.springframework.boot.jdbc.autoconfigure.DataSourcePropertiesTests` | 1 of 16 (`determineUrlWithNoEmbeddedSupport`) |
| `module/spring-boot-jdbc` | `org.springframework.boot.jdbc.autoconfigure.DataSourceAutoConfigurationTests` | 1 of 24 (`testBadUrl`) |

`DataSourceAutoConfigurationTests` has a second, unrelated failure in this
same run (`whenThereIsAnEmptyUserProvidedDataSource` — the context starts
successfully with 13 beans but the `DataSource` bean assertion sees an
unexpected value) that does **not** fit this cluster (that test's sibling
using the same `hideConnectionPools()` loader passes, and the failure shape
— a started context, not a bad-URL/wrong-type mismatch — differs). Not
confidently clustered; left out of this doc and not filed separately.
