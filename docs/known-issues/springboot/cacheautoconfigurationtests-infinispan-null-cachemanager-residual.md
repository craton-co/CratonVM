# `CacheAutoConfigurationTests` Infinispan-provider tests: "Expecting actual not to be null" after context starts successfully

**Status: OPEN — found 2026-07-17**, while re-verifying
[`contextrunner-resource-cycle-then-silent-stall-cluster-FIXED.md`](../../internal/springboot/contextrunner-resource-cycle-then-silent-stall-cluster-FIXED.md).

## Symptom

`module/spring-boot-cache`'s `CacheAutoConfigurationTests` (given enough wall
time — see cross-reference below) completes with 5/59 tests failing, all 5
exercising the `org.infinispan.jcache`/`org.infinispan.spring.embedded`
Infinispan cache provider:

| Test method | Failure |
|---|---|
| `infinispanAsJCacheWithCaches` | `IllegalStateException: Unstarted application context [...][startupFailure=UnsatisfiedDependencyException] failed to start` |
| `infinispanAsJCacheWithConfig` | same shape |
| `infinispanCacheWithCaches` | `AssertionError: Expecting actual not to be null` |
| `infinispanCacheWithConfig` | same shape |
| `infinispanCacheWithCachesAndCustomConfig` | same shape |

The `infinispanCacheWith*` group's failures are all a plain AssertJ
`Expecting actual not to be null` at the point the test asserts on the
resolved `CacheManager`/cache bean — i.e. the `ApplicationContext` itself
starts successfully (no `BeanCreationException`), but the specific bean the
test expects is absent or null where the test dereferences it.

## How this was found

Found while re-verifying
[`contextrunner-resource-cycle-then-silent-stall-cluster-FIXED.md`](../../internal/springboot/contextrunner-resource-cycle-then-silent-stall-cluster-FIXED.md) —
`CacheAutoConfigurationTests` was one of the 5 classes originally
mis-filed there as a hung/leaked-resource-deadlock candidate. Running it
standalone with a generous timeout showed it completes normally (no
deadlock); with the concurrently-landed `Class.getMethods()`
override-shadowing fix (`b6f38f669`/`42e0717ac`) applied, the original
14 destroy-method-ambiguity failures dropped to these 5 unrelated,
Infinispan-specific ones.

## Root cause — NOT investigated this session

Not root-caused; flagged as a residual for a follow-up session. Worth
checking first whether `org.infinispan.jcache.embedded.JCachingProvider`/
`org.infinispan.spring.embedded.provider.SpringEmbeddedCacheManager`
registration depends on a native/reflection path already known to be gappy
elsewhere in this cluster's investigation (e.g. the `.proto` schema
lexer/parser path exercised during Infinispan startup — a standalone probe
of `SimpleCharStream`/`ProtoParserTokenManager` against a small `.proto`
string completed correctly under this build, so the schema *parser* itself
is not obviously implicated; the gap is more likely a Spring
condition-evaluation or bean-resolution difference specific to the
`infinispanCacheWith*` configuration path).

## Affected class

| Module | Class |
|---|---|
| `module/spring-boot-cache` | `org.springframework.boot.cache.autoconfigure.CacheAutoConfigurationTests` (5 of 59 test methods, all Infinispan-provider-specific) |
