# `CacheAutoConfigurationTests` Hazelcast JCache tests: `AbstractMethodError` on `java.net.http.HttpRequest.method()`

**Status: FIXED — 2026-07-20.**

## Symptom

`module/spring-boot-cache`'s `CacheAutoConfigurationTests`, after the
Infinispan `getCacheNames()` null-return bug in the sibling doc was fixed,
still showed 2/59 failures — both exercising Hazelcast as a JSR-107
(`javax.cache`) provider:

| Test method | Failure |
|---|---|
| `hazelcastAsJCacheWithCaches` | `UnsatisfiedDependencyException` → ... → `AbstractMethodError: method java/net/http/HttpRequest.method()Ljava/lang/String; has no Code attribute` |
| `jCacheCacheWithCachesAndCustomizer` | same shape (also uses `HazelcastServerCachingProvider`) |

Both failed inside `HazelcastServerCachingProvider.getDefaultInstance()` →
`Hazelcast.getOrCreateHazelcastInstance()` → Node/discovery-service startup
→ `AzureDiscoveryStrategyFactory.isEndpointAvailable()` →
`com.hazelcast.spi.utils.RestClient.call(HttpRequest)`, which invokes
`request.method()` on a built `java.net.http.HttpRequest` purely for its own
logging/retry bookkeeping.

## Root cause

`native-builtins/src/net_phase_e.rs`'s `register_re5_http_client` — the
**active, always-registered** (real-JDK-mode) HTTP client subsystem —
builds `HttpRequest` objects via `HttpRequest$Builder.build()` by allocating
them directly as class `java/net/http/HttpRequest`, the **abstract JDK class
itself**, not a concrete subclass (`alloc_concurrent_synthetic(ctx,
"java/net/http/HttpRequest", 5)`). This is a legitimate, established pattern
in this codebase (a real, functioning Rust-backed HTTP client wearing the
JDK's real class name), but it registered natives for only some of
`HttpRequest`'s public instance getters (`timeout()`) — not `method()` or
`uri()`. Real `java.net.http.HttpRequest.method()` is declared `abstract`
(no `Code` attribute) since real JVMs never instantiate it directly (only
concrete subclasses like `jdk.internal.net.http.ImmutableHttpRequest`). Any
real Java bytecode calling `.method()` on an instance whose runtime class is
the bare abstract `HttpRequest` therefore hits abstract-method resolution
with no override anywhere in the hierarchy → `AbstractMethodError: ... has
no Code attribute` — confirmed independently of Spring/Hazelcast with a
3-line probe (`HttpRequest.newBuilder()...build()` then `.method()`).

A second, separate `getCacheNames()`-style native override for the exact
same class/method DOES already exist — `native-builtins/src/http2.rs`'s
`register_http_request` — but that whole subsystem (`register_http2_natives`,
reachable only via `register_synthetic_overrides`) is `#[cfg(feature =
"synthetic-jdk")]`-gated and therefore dead code in the real-JDK CLI build
used by the Spring Boot suite (and by ordinary CratonVM usage with
`--java-home`). Two independent HTTP-request object models exist in this
codebase; only one is reachable in the build under test, and it was missing
the getter.

## Fix

`native-builtins/src/net_phase_e.rs`: added `method()`/`uri()` getter
registrations directly on the `java/net/http/HttpRequest` class (alongside
the existing `timeout()` getter), reading back the same fields the builder
already populates (`0`=method `String`, `1`=uri `String`) — `method()`
returns the stored string (defaulting to `"GET"`), `uri()` reconstructs a
real `java.net.URI` via `URI.create(String)`.

Verified: the isolated probe now prints the correct method (`GET`/`POST`)
instead of throwing. Full `CacheAutoConfigurationTests`: 59/59 pass
(previously 57/59), stable across two consecutive runs. `net_phase_e`'s 40
existing unit tests unaffected.

## Affected class

| Module | Class |
|---|---|
| `module/spring-boot-cache` | `org.springframework.boot.cache.autoconfigure.CacheAutoConfigurationTests` (2 of 59 test methods, both Hazelcast-JCache-provider-specific) |
