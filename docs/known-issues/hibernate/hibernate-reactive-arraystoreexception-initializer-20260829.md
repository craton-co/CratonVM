# `ArrayStoreException: org.hibernate.sql.results.graph.Initializer` — 9 hibernate-reactive classes, one identical signature

## Status
New finding, 2026-08-29, hibernate-reactive complete-suite run (Postgres via
Testcontainers). All 9 classes share the byte-identical exception. Not yet
cross-checked against HotSpot, not yet root-caused inside CratonVM.

## Symptom

Nine classes, all in the embeddable/embedded-id mapping family, fail with the
same exception:

```
org.hibernate.reactive.EagerElementCollectionForEmbeddableEntityTypeMapTest
org.hibernate.reactive.EagerElementCollectionForEmbeddableTypeListTest
org.hibernate.reactive.EagerElementCollectionForEmbeddedEmbeddableMapTest
org.hibernate.reactive.EagerElementCollectionForEmbeddedEmbeddableTest
org.hibernate.reactive.EagerOrderedElementCollectionForEmbeddableTypeListTest
org.hibernate.reactive.EmbeddedIdTest
org.hibernate.reactive.EmbeddedIdWithManyEagerTest
org.hibernate.reactive.EmbeddedIdWithManyTest
org.hibernate.reactive.EmbeddedIdWithOneToOneTest
```

```
java.lang.ArrayStoreException: org.hibernate.sql.results.graph.Initializer
	at org.hibernate.reactive.sql.exec.internal.StandardReactiveSelectExecutor.doExecuteQuery(StandardReactiveSelectExecutor.java:195)
	at org.hibernate.reactive.sql.exec.internal.StandardReactiveSelectExecutor.executeQuery(StandardReactiveSelectExecutor.java:163)
	at org.hibernate.reactive.sql.exec.internal.StandardReactiveSelectExecutor.executeQuery(StandardReactiveSelectExecutor.java:131)
	at org.hibernate.reactive.sql.exec.internal.StandardReactiveSelectExecutor.list(StandardReactiveSelectExecutor.java:109)
	at org.hibernate.reactive.query.sqm.internal.ConcreteSqmSelectReactiveQueryPlan.lambda$listInterpreter$1(ConcreteSqmSelectReactiveQueryPlan.java:102)
```
(wrapped in `java.util.concurrent.CompletionException` on most, bare on
`EmbeddedIdWithManyTest`.)

## Why this looks like a real defect, not app-level noise

`ArrayStoreException` is thrown by the JVM's own `aastore` bytecode when
storing into a covariant array whose runtime component type rejects the
value being stored — it's a low-level array-type-check failure, not
something application code raises directly. It surfacing specifically
against `org.hibernate.sql.results.graph.Initializer` inside
`StandardReactiveSelectExecutor.doExecuteQuery` suggests Hibernate Reactive
builds/populates an `Initializer[]`-typed (or similarly covariant) array
during result-graph construction, and something about that store is being
rejected.

Two candidate explanations, not distinguished yet:
1. **A genuine CratonVM defect in array-store type checking** (`aastore`'s
   component-type check) — this session has an established pattern of
   defects in adjacent machinery (compiled `checkcast`, at 136x cost, was a
   confirmed and fixed defect; array KIND_TAGS handling has been a recurring
   theme). An incorrect array-store check would be a related but distinct
   bug from checkcast.
2. **A correct rejection of a real Hibernate Reactive bug** that HotSpot
   happens not to trigger because of some other behavioral difference
   upstream (e.g. a different code path taken due to a CratonVM-specific
   quirk elsewhere, which then constructs the array differently before the
   store).

## Not yet done
- HotSpot A/B on one representative class (e.g. `EmbeddedIdTest`) — the
  single fastest way to confirm CratonVM-specificity.
- Identify exactly which array and which element type are involved — a
  `CRATONVM_DBG_LAYOUT=1`-style trace or a targeted breakpoint on the
  `doExecuteQuery` call site to see the actual declared vs. actual runtime
  types at the failing `aastore`.
- A minimal standalone repro (`Initializer[] arr = ...; arr[i] = someInitializerSubtype;`)
  once the actual array/element shape is known, to isolate this from the
  full Hibernate Reactive/Postgres/Testcontainers stack.
- Whether this reproduces on H2 too, or is Postgres/reactive-driver-specific
  (this batch ran against Postgres via Testcontainers).

## Repro

```bash
cd apps/hibernate-reactive-suite-runner
cratonvm.exe --java-home <jdk25> -XX:+UseZGC <hibernate-reactive classpath+args> \
  CratonRunner org.hibernate.reactive.EmbeddedIdTest
# needs a live Postgres reachable via Testcontainers/Docker (see DB-REQUIRED.md)
```
