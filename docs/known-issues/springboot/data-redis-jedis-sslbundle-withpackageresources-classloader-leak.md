# `DataRedisAutoConfigurationJedisTests.testRedisConfigurationWithSslBundle` — `@WithPackageResources` classloader leak — OPEN

## Symptom

`DataRedisAutoConfigurationJedisTests` (`@ClassPathExclusions("lettuce-core-*.jar")`)
is 22/23 `PASS` on CratonVM. The one failure,
`testRedisConfigurationWithSslBundle` (also annotated
`@WithPackageResources("test.jks")`), fails with:

```
BeanCreationException: Error creating bean with name 'redisConnectionFactory'
  defined in class path resource [.../LettuceConnectionConfiguration.class]:
Failed to instantiate [LettuceConnectionFactory]:
  Factory method 'redisConnectionFactory' threw exception with message:
  io.lettuce.core.SslVerifyMode
Caused by: java.lang.NoClassDefFoundError: io.lettuce.core.SslVerifyMode
```

Confirmed **cratonvm-specific**: HotSpot passes 23/23 (6.2s total,
`redis-jedis-hotspot-20260723` run). Confirmed **deterministic and
isolated** (not cross-test contamination): fails identically when run as
the *only* method in the class via `run-single-method.ps1`
(`tests=1 failed=1`).

## Why this is surprising

The test class excludes `lettuce-core-*.jar` from its classpath
specifically so `@ConditionalOnClass(RedisClient.class)` on
`LettuceConnectionConfiguration` evaluates false and the Lettuce
auto-configuration is skipped entirely, falling through to
`JedisConnectionConfiguration`. This works correctly for every *other* test
in the class (all of which also rely on the same exclusion + fallback,
several with SSL properties too, e.g.
`testRedisConfigurationWithSslDisabledAndBundle` passes). Only the ONE test
combining `@ClassPathExclusions` (class-level) with `@WithPackageResources`
(method-level) fails — and it fails with `LettuceConnectionConfiguration`'s
own `redisConnectionFactory()` bean method actually *executing* (not
skipped), meaning `@ConditionalOnClass(RedisClient.class)` did not gate it
out for this one test.

## Leading hypothesis (not yet confirmed with a debugger/trace)

`@WithPackageResources` is backed by
`org.springframework.boot.testsupport.classpath.resources.ResourcesExtension`
(`test-support/spring-boot-test-support/.../ResourcesExtension.java`).
Its `beforeEach`:

```java
ResourcesClassLoader classLoader = new ResourcesClassLoader(
    context.getRequiredTestClass().getClassLoader(), resources);
Thread.currentThread().setContextClassLoader(classLoader);
```

This calls `getRequiredTestClass().getClassLoader()` — i.e. asks the JVM
which classloader actually *defined* `DataRedisAutoConfigurationJedisTests`
— and wraps **that** loader as the parent of a new `ResourcesClassLoader`,
installed as the new thread-context classloader for the test.

On HotSpot this correctly returns the exclusion-aware
`ModifiedClassPathClassLoader` instance that defined the test class (since
`ModifiedClassPathExtension` swaps the classloader before the test class is
even loaded), so the wrapped loader's parent chain still excludes
`lettuce-core.jar`.

**Hypothesis:** on CratonVM, `Class.getClassLoader()` for a class defined
via the `URLClassLoader.findClass` fast path
(`classloader.rs::ucl_try_define_local_class`, which does call
`register_defining_loader(cid, loader)` — so the plumbing exists) does not
faithfully return that same defining-loader instance in this specific
scenario, so the newly-built `ResourcesClassLoader` ends up wrapping a
*different* (non-excluded, full-classpath) loader. Spring's
`ApplicationContextRunner`/bean-creation machinery consults
`Thread.currentThread().getContextClassLoader()` in several places, so
`RedisClient`/`SslVerifyMode` become reachable through that leaked parent —
matching the observed `@ConditionalOnClass` bypass and the subsequent
`NoClassDefFoundError` (Lettuce's `SslOptions.builder()` static defaults
reference `SslVerifyMode`, and *partial* linkage against the wrong loader's
half-visible lettuce classes is a plausible way to get `NoClassDefFoundError`
rather than a clean `ClassNotFoundException`).

**Not yet verified**: this needs either (a) a `CRATONVM_DBG_CLASSLOADER`-
style trace of `loader_namespace_id`/`register_defining_loader` around this
specific test's `getClassLoader()` call, or (b) a minimal standalone repro
(no Spring/Redis) that defines a class via a custom `URLClassLoader` with an
excluded package, then constructs a second wrapping loader from
`thatClass.getClassLoader()` and checks whether the exclusion survives.

## Status

Pre-existing (not introduced by the `data-redis` HANG fix landed
2026-07-23, see `data-redis-urlclassloader-uncached-classpath-hang-FIXED.md`
in this directory) — previously masked because the whole class HANGed
before completing any test. Now visible and reproducible. OPEN — root
cause narrowed to `Class.getClassLoader()`/defining-loader identity under
`ResourcesExtension`'s wrap-and-install pattern, but not yet fixed.

## Reproduce

```powershell
$exe = "<worktree>\target\release\<binary>.exe"
powershell.exe -NoProfile -ExecutionPolicy Bypass -File apps\spring-boot-suite-runner\run-single-method.ps1 `
  -Module "module/spring-boot-data-redis" `
  -ClassName "org.springframework.boot.data.redis.autoconfigure.DataRedisAutoConfigurationJedisTests" `
  -Method "testRedisConfigurationWithSslBundle" `
  -Exe $exe -JdkHome "C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot"
```
