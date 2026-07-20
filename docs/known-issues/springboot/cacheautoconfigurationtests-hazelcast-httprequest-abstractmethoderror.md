# `CacheAutoConfigurationTests` Hazelcast JCache tests: `AbstractMethodError` on `java.net.http.HttpRequest.method()`

**Status: OPEN — found 2026-07-19**, while verifying
[`cacheautoconfigurationtests-infinispan-null-cachemanager-residual.md`](../../internal/springboot/cacheautoconfigurationtests-infinispan-null-cachemanager-FIXED.md).

## Symptom

`module/spring-boot-cache`'s `CacheAutoConfigurationTests`, after the
Infinispan `getCacheNames()` null-return bug above was fixed, still shows 2/59
failures — both exercising Hazelcast as a JSR-107 (`javax.cache`) provider:

| Test method | Failure |
|---|---|
| `hazelcastAsJCacheWithCaches` | `UnsatisfiedDependencyException` → ... → `AbstractMethodError: method java/net/http/HttpRequest.method()Ljava/lang/String; has no Code attribute` |
| `jCacheCacheWithCachesAndCustomizer` | same shape (also uses `HazelcastServerCachingProvider`) |

Both fail inside `JCacheCacheConfiguration.jCacheCacheManager`'s call to
`cachingProvider.getCacheManager(...)`, which for Hazelcast's
`HazelcastServerCachingProvider` apparently constructs an internal HTTP
client (`java.net.http.HttpRequest`) — some concrete/synthetic
implementation of `HttpRequest.method()` reachable from that path has no
`Code` attribute under CratonVM (i.e. the method resolved is abstract or a
native-stub placeholder rather than the real implementing class').

## Root cause — NOT investigated this session

Not root-caused. Confirmed pre-existing and **unrelated** to the Infinispan
`getCacheNames()` bug in the sibling doc above — this failure shape was
present identically both before and after that fix (it only became visible
once a correct Gradle-generated classpath let the full class run at all).
Worth checking first whether this is the same family as other
`java.net.http`-related native-registration gaps already tracked elsewhere
in this codebase (`native-builtins/src/http_client.rs`,
`native-builtins/src/http2.rs`) — likely a concrete `HttpRequest`
implementation class (e.g. Hazelcast's own request builder, or a JDK
internal `HttpRequestImpl`) missing a native override/real bytecode wiring
for `method()`.

## Affected class

| Module | Class |
|---|---|
| `module/spring-boot-cache` | `org.springframework.boot.cache.autoconfigure.CacheAutoConfigurationTests` (2 of 59 test methods, both Hazelcast-JCache-provider-specific) |
