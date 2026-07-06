# Infinispan real Cache objects get a synthetic (fieldless) Cache from native_dcm_get_cache, causing `cache.config` NPEs

Status: open

Date observed: 2026-07-06 — surfaced as the residual once
[keycloak-model-jgroupstransport-start-never-invoked-FIXED](../internal/fixed-suite-bugs/keycloak-model-jgroupstransport-start-never-invoked-FIXED.md)
was fixed (that fix let `DefaultCacheManager.start()` actually run its real
`internalStart(boolean)` — `GlobalComponentRegistry`, `JGroupsTransport`,
etc. — for a real, non-shimmed `DefaultCacheManager`, instead of being
silently swallowed by a legacy synthetic native).

## Summary

Same bug *family* as the one just fixed: `native-builtins/src/infinispan_local.rs`'s
"T19.10" synthetic local-cache backend registers natives for
`DefaultCacheManager`/`Cache`/`CacheImpl`/`CacheAdvanced` (`getCache`,
`put`, `get`, `remove`, `containsKey`, `size`, `clear`, `evict`,
`addListener`, `removeListener`, `getName`, `putIfAbsent`, `replace`, etc.)
**unconditionally** — native dispatch is keyed by class+method+descriptor,
not by which constructor built the receiver. `native_dcm_start`/`stop` had
exactly this bug (now fixed via `is_real_dcm()`); `native_dcm_get_cache`
(and presumably the `Cache`-instance-method natives) still do.

Once `DefaultCacheManager.start()` correctly runs the real
`internalStart(boolean)` (per the fix above), Infinispan's own internal
bootstrap code (`GlobalComponentRegistry.postStart()` →
`GlobalConfigurationManagerImpl.postStart()`) iterates real, internal caches
and calls `.entrySet()` on them. That call reaches
`AbstractCacheBackedSet.<init>`, which reads `cache.config.clustering()` —
and `cache.config` is null, because the `Cache` object it's operating on is
one that `native_dcm_get_cache` fabricated: a **synthetic** object (only 3
fields ever written: `handle`/`name`/`manager`, backed by a Rust-side
`CacheInner` HashMap) even though it was allocated with the REAL `CacheImpl`
class's full field layout (so `config`, along with every other real field,
sits at its zero-initialized default — `null`).

```text
Caused by: java.lang.NullPointerException: Cannot invoke "org.infinispan.configuration.cache.Configuration.clustering()" because "cache.config" is null
    org.infinispan.cache.impl.AbstractCacheBackedSet.<init>(AbstractCacheBackedSet.java:54)
    org.infinispan.cache.impl.CacheBackedEntrySet.<init>(CacheBackedEntrySet.java:22)
    org.infinispan.cache.impl.CacheImpl.entrySet(CacheImpl.java:851)
    org.infinispan.cache.impl.CacheImpl.entrySet(CacheImpl.java:847)
    org.infinispan.cache.impl.CacheImpl.entrySet(CacheImpl.java:139)
    java.util.concurrent.ConcurrentMap.forEach(ConcurrentMap.java:112)
    org.infinispan.globalstate.impl.GlobalConfigurationManagerImpl.postStart(GlobalConfigurationManagerImpl.java:128)
    org.infinispan.CoreModule.cacheManagerStarted(CoreModule.java:19)
    org.infinispan.factories.GlobalComponentRegistry.modulesManagerStarted(GlobalComponentRegistry.java:326)
    org.infinispan.factories.GlobalComponentRegistry.postStart(GlobalComponentRegistry.java:300)
    org.infinispan.manager.DefaultCacheManager.internalStart(DefaultCacheManager.java:707)
```

## Not yet root-caused / fixed

Unlike `start()`/`stop()` (which just delegate to a single no-arg/simple-arg
internal method), `getCache(String)`'s real bytecode
(`internalGetCache(String)`) likely does substantially more — real cache
creation, wiring, and interceptor-chain setup — so simply delegating via
`ctx.invoke_virtual(this, "internalGetCache", ...)` the same way the
`start()`/`stop()` fix does may or may not be sufficient; it needs the same
`is_real_dcm()`-style guard (reusable directly, it's already defined in
`infinispan_local.rs`) plus verification that `internalGetCache`'s own
transitive real-bytecode dependencies don't hit further native gaps of their
own (plausible, given how many were found one layer at a time while fixing
the `start()` issue — `UnixDispatcher.close0`, `NativeSocketAddress`'s 12
native probes).

Whoever picks this up should also check whether the OTHER Cache-instance
natives (`put`/`get`/`remove`/`containsKey`/`size`/`clear`/`evict`/
`addListener`/`removeListener`/`getName`/`putIfAbsent`/`replace` — all
registered for `CLS_CACHE`/`CLS_CACHE_IMPL`/`CLS_CACHE_ADVANCED`) need the
same treatment, or whether it's acceptable/intentional for cache *contents*
operations to stay on the synthetic Rust-side `CacheInner` backend even for
a real `DefaultCacheManager` (i.e., only `getCache()` itself needs to return
a genuinely real `Cache` object with a real `config`/interceptor chain, and
the put/get/remove natives could keep working against the synthetic
`CacheInner` if reads/writes are keyed consistently by cache name — TBD,
depends on whether real Infinispan features Keycloak relies on for these
caches, e.g. listeners/eviction/expiry, need the real interceptor chain to
function correctly).

## Repro

Requires the *real* Keycloak-resolved dependency versions (Infinispan
16.0.8, Netty 4.1.132.Final, JGroups 5.5.1.Final). See the FIXED doc this
one supersedes for the full classpath-generation recipe. Run
`org.keycloak.testsuite.model.RealmModelTest` via `KcRunner`, or the minimal
`ISPNProbe3.java` repro extended to call `cm.getCache(...)` after
`start()`/`getChannel()` succeed.

Currently: `FAIL` — real, deterministic `NullPointerException` in
`GlobalConfigurationManagerImpl.postStart()`, reached during
`DefaultCacheManager.internalStart()`'s own internal bootstrap (not
something Keycloak's code explicitly triggers), blocking all 37
`testsuite/model` classes from getting past `createEmbeddedCacheManager`.
