# WebFlux `DefaultPathContainer`: cached `DefaultSeparator` value fails `checkcast` to its own declared type

**Status: OPEN — found 2026-07-28**

## Symptom

| Module | Class | Failing methods |
|---|---|---|
| `module/spring-boot-graphql` | `org.springframework.boot.graphql.autoconfigure.reactive.GraphQlWebFluxAutoConfigurationTests` | `shouldExposeGraphiqlEndpoint`, `shouldConfigureWebInterceptors`, `shouldRejectMissingQuery`, `SseSubscriptionShouldWork` (4 of 17) |

All four failures are the identical exception, differing only in which test
triggered it:

```
=> org.springframework.web.reactive.function.client.WebClientRequestException: java.lang.Object cannot be cast to org.springframework.http.server.DefaultPathContainer$DefaultSeparator
       org.springframework.web.reactive.function.client.ExchangeFunctions$DefaultExchangeFunction.lambda$wrapException$0(ExchangeFunctions.java:139)
       reactor.core.publisher.MonoErrorSupplied.subscribe(MonoErrorSupplied.java:56)
       ...
       Suppressed: java.lang.Exception: #block terminated with an error
         org.springframework.test.web.reactive.server.DefaultWebTestClient$DefaultRequestBodyUriSpec.exchange(DefaultWebTestClient.java:375)
```

Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260728/shard1/logs/module_spring-boot-graphql.org.springframework.boot.graphql.autoconfigure.reactive.GraphQlW-6887be131002.out.log`

`WebClientRequestException` here is just a wrapper — the real failure is a
`ClassCastException` raised somewhere in the reactive request path (client
or in-process server routing) while the test's `WebTestClient` exchanges a
request against the auto-configured GraphQL endpoint.

## Root cause (confirmed via javap disassembly of the real jar)

`org.springframework.http.server.DefaultPathContainer`
(`spring-web-7.1.0-SNAPSHOT.jar`) keeps a small, `<clinit>`-populated cache:

```java
private static final Map<Character, DefaultPathContainer.DefaultSeparator> SEPARATORS;
```

populated by `new DefaultSeparator(...)` calls in the static initializer
(confirmed via `javap -c`, offsets 20-44 of `<clinit>`), and read back
elsewhere in the class via `SEPARATORS.get(c)` followed by an explicit
`checkcast DefaultSeparator` (`Map.get` erases to `Object`, so the generated
bytecode always casts the retrieved value back to the map's declared value
type before use).

The observed `ClassCastException: java.lang.Object cannot be cast to
DefaultSeparator` means that `checkcast` failed against a value that was
*put into the map by `new DefaultSeparator(...)` in the very same class's
own `<clinit>`* — i.e. an object whose class should be trivially,
unambiguously `DefaultSeparator`. `DefaultSeparator` itself is an ordinary,
non-generic, 2-field class (`separator: String`, `encodedSequence: String`)
implementing one interface (`PathContainer$Separator`) — nothing about its
shape suggests it should hit any of this codebase's already-documented
"compact/synthetic field-slot" class-identity gaps.

**Not root-caused to a specific CratonVM file:line this session** (no
build/test execution performed as part of this task — log-analysis and
source-reading only). The two most plausible mechanisms, neither confirmed:

1. **A GC-safety/relocation bug specific to values retained inside a
   `HashMap`** built once at class-init time and read repeatedly from many
   different (here, reactive/Reactor-scheduler) threads later — if a moving
   GC cycle relocates the `DefaultSeparator` singleton instances but the
   map's stored reference (or its class-identity metadata) isn't correctly
   updated, a later reader could see a corrupted/stale reference whose
   apparent class no longer matches its real declaring class. This class of
   bug (class-identity/metadata corruption surviving a GC move) has
   precedent elsewhere in this codebase (see
   `reference_synthetic_carrier_metadata_misreport.md`,
   `reference_postgc_stale_local_selfforward_falsepos.md`), though those
   are documented for different concrete call sites, not this one.
2. **A duplicate-class-copy issue**: if `DefaultPathContainer$DefaultSeparator`
   somehow gets loaded/defined twice under two different loader identities
   reachable from the same reactive pipeline (e.g. once from the
   application's real classpath, once from an isolated/forked classpath a
   different part of the WebFlux/GraphQL auto-configuration touches), a
   value produced by one copy's `<clinit>` would legitimately fail
   `checkcast` against the other copy's `Class` object — this is the normal,
   expected behavior for duplicate class copies under isolating loaders
   (see `reference_multiple_class_copies_are_normal_under_isolating_loaders.md`),
   so if this is the mechanism, the actual bug would be *why* two copies of
   this specific class end up sharing one request's server-side routing
   path.

## Confirming/refuting this hypothesis

Add a temporary trace on `DefaultSeparator`'s `<init>`/on the failing
`checkcast` site (or run under `--nojit` to rule out a JIT-specific angle)
and rerun `GraphQlWebFluxAutoConfigurationTests`. If the object's
`getClass().getClassLoader()` differs from `DefaultPathContainer.class
.getClassLoader()` at the failure site, that confirms mechanism 2; if both
loaders match, mechanism 1 (or an as-yet-unidentified third mechanism)
becomes more likely.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-graphql` | `org.springframework.boot.graphql.autoconfigure.reactive.GraphQlWebFluxAutoConfigurationTests` |
