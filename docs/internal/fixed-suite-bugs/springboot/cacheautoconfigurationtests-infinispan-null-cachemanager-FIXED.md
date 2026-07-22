# `CacheAutoConfigurationTests` Infinispan-provider tests: "Expecting actual not to be null" after context starts successfully

**Status: FIXED — 2026-07-19.**

## Symptom (original)

`module/spring-boot-cache`'s `CacheAutoConfigurationTests` (given enough wall
time — see cross-reference below) completed with 5/59 tests failing, all 5
exercising the `org.infinispan.jcache`/`org.infinispan.spring.embedded`
Infinispan cache provider:

| Test method | Failure |
|---|---|
| `infinispanAsJCacheWithCaches` | `IllegalStateException: Unstarted application context [...][startupFailure=UnsatisfiedDependencyException] failed to start` |
| `infinispanAsJCacheWithConfig` | same shape |
| `infinispanCacheWithCaches` | `AssertionError: Expecting actual not to be null` |
| `infinispanCacheWithConfig` | same shape |
| `infinispanCacheWithCachesAndCustomConfig` | same shape |

## Root cause

`native-builtins/src/infinispan_local.rs` implements `DefaultCacheManager` as
a **synthetic native overlay** (built for Keycloak's local-mode caching) that
intercepts `getCache`, `cacheExists`, `defineConfiguration`, `start`/`stop`
**and `getCacheNames()`** on both the 3-field synthetic-construction path
*and* the real, non-intercepted constructors (native dispatch is keyed by
class+method+descriptor, not by which constructor built the receiver — see
`is_real_dcm`'s doc in that file). Every other intercepted method already had
an `is_real_dcm(ctx, this)` branch that delegated to a reimplementation of
the real bytecode logic for genuinely-real managers (e.g.
`native_real_dcm_cache_exists`). `native_dcm_get_cache_names` did not: it
unconditionally returned `Object(None)` (Java `null`), regardless of whether
`this` was the Keycloak-style synthetic object or a fully real
`DefaultCacheManager` constructed via `new DefaultCacheManager(InputStream)`
or the plain no-arg/`GlobalConfiguration` constructors that Spring Boot's
`InfinispanCacheConfiguration`/`JCacheCacheConfiguration` use.

Real Infinispan's own `DefaultCacheManager.getCacheNames()` bytecode can
never return `null` (it always returns `Collections.emptySet()` or an
`Immutables`-wrapped `Set`), so downstream real Infinispan code assumes this
and never null-checks the result. Two different call sites hit that
assumption:

- `SpringEmbeddedCacheManager.getCacheNames()` (Spring's own
  `CacheManager.getCacheNames()` contract) delegates straight through to
  `nativeCacheManager.getCacheNames()` with no guard — surfaced as
  `AssertionError: Expecting actual not to be null` in the
  `infinispanCacheWith*` (non-JCache) tests.
- `org.infinispan.jcache.embedded.JCacheManager`'s internal
  `registerPredefinedCaches()` (reached via the JSR-107
  `JCachingProvider.createCacheManager` path Spring Boot's
  `JCacheCacheConfiguration` uses) does `cm.getCacheNames().iterator()`
  directly — surfaced as `NullPointerException: Cannot invoke
  "java.util.Set.iterator()" because "cacheNames" is null` (wrapped by Spring
  as an `UnsatisfiedDependencyException`/`IllegalStateException: Unstarted
  application context ... failed to start`) in the `infinispanAsJCacheWith*`
  tests.

Confirmed in isolation with a minimal 3-line Infinispan probe (`new
DefaultCacheManager()` + `defineConfiguration("foo"/"bar")` +
`getCacheNames()`), independent of Spring entirely: real JDK/HotSpot printed
`[bar, foo]`, CratonVM printed `null`.

A second, smaller gap surfaced once the primary null-return was fixed: the
synthetic (Keycloak-style) branch's `cache_names()` only listed
already-**instantiated** caches (`self.caches`, populated lazily on the first
`get_cache()` call), not caches registered via `defineConfiguration` alone —
real Infinispan's `getCacheNames()`/`getDefinedCaches()` reflects
configuration, not instantiation. This left `infinispanCacheWithCaches`/
`infinispanCacheWithCachesAndCustomConfig` still returning an empty (not
null, but incomplete) set, since Spring's `InfinispanCacheConfiguration`
calls `defineConfiguration()` per configured cache name but never calls
`getCache()` on them up front.

## Fix

`native-builtins/src/infinispan_local.rs`:

1. `native_dcm_get_cache_names` now branches on `is_real_dcm`:
   - **Real manager**: new `native_real_dcm_get_cache_names` reimplements the
     real bytecode logic via `invoke_virtual` against the real fields —
     `configurationManager.getDefinedCaches()` seeded into a real
     `java.util.TreeSet`, unioned with `caches.keySet()`, filtered through
     `globalComponentRegistry.getComponent(InternalCacheRegistry.class)
     .filterPrivateCaches(...)` — mirroring the pattern already established by
     `native_real_dcm_cache_exists`/`native_dcm_get_cache`.
   - **Synthetic manager**: returns the real names from
     `global_manager().cache_names()` (built into a real `java.util.HashSet`
     via the existing `build_real_layout_string_hashset` helper) instead of
     `null`.
2. `DefaultCacheManagerInner::cache_names()` now unions `self.caches.keys()`
   (instantiated) with `self.pending_configs.keys()` (defined via
   `defineConfiguration` but not yet instantiated), matching real
   Infinispan's configuration-based (not instantiation-based) semantics.

Verified: all 5 originally-failing Infinispan test methods now pass. Full
`CacheAutoConfigurationTests` re-run: 59 tests, 57 passed / 2 failed — the 2
remaining failures are an unrelated, pre-existing bug (Hazelcast's JCache
client hitting `AbstractMethodError: method
java/net/http/HttpRequest.method()Ljava/lang/String; has no Code attribute`),
present before this fix too and unrelated to Infinispan/cache-names — see
[`cacheautoconfigurationtests-hazelcast-httprequest-abstractmethoderror.md`](../../known-issues/springboot/cacheautoconfigurationtests-hazelcast-httprequest-abstractmethoderror.md).
`cratonvm-native-builtins`'s existing 23 `infinispan_local` unit tests
(including `t19_10_cache_names_listing`) still pass unmodified.

## How this was found

Found while re-verifying
[`contextrunner-resource-cycle-then-silent-stall-cluster-FIXED.md`](cle-then-silent-stall-cluster-FIXED.md) —
`CacheAutoConfigurationTests` was one of the 5 classes originally mis-filed
there as a hung/leaked-resource-deadlock candidate.

## Affected class

| Module | Class |
|---|---|
| `module/spring-boot-cache` | `org.springframework.boot.cache.autoconfigure.CacheAutoConfigurationTests` (5 of 59 test methods, all Infinispan-provider-specific) |
