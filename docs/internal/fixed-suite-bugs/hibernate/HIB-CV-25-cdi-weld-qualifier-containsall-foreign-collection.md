# HIB-CV-25 — CDI/Weld `WELD-001301` — `HashSet.containsAll(foreignCollection)` false-positive ✅ FIXED

**Status:** FIXED on dev (native-collections; opt-out N/A — pure correctness fix)
**Severity:** High — broke ~all CDI (Weld) bootstrap; deterministic under `--nojit`, HotSpot PASS
**Area:** `native-collections` — HashSet bulk-op argument extraction (NOT annotation reflection)

---

## Symptom

The `org.hibernate.orm.test.cdi.*` family failed to bootstrap CDI (Weld 6.0.4):

```
org.jboss.weld.exceptions.IllegalArgumentException: WELD-001301: Annotation
QualifierInstance {annotationClass=interface jakarta.enterprise.inject.Produces,
values={}} is not a qualifier
```

`@Produces` is **not** a qualifier, yet Weld built a `QualifierInstance` for it and
its own validation rejected it.

## The original hypothesis was WRONG

The first triage guessed CratonVM's **meta-annotation reflection** on annotation
types was wrong (`Produces.class.getAnnotations()` / `isAnnotationPresent(Qualifier)`
/ `getMetaAnnotations(Qualifier)`). **Every one of those was verified byte-identical
to HotSpot**, even through Weld's real `HotspotReflectionCache` / `ClassTransformer`
and a real `EnhancedAnnotatedMethod`. The producer's `getMetaAnnotations(Qualifier)`
correctly returned `{@Parameters}`.

## Real root cause

The single producer in the minimal deployment is Weld SE's
`ParametersFactory.getArgs()`, annotated `@Produces @Parameters` (both member-less,
`hashCode()==0`). Its computed qualifier set is `{@Parameters, @Any}`. But
`BeanAttributesFactory.initQualifiers` ends with
`qualifiers = SharedObjectCache.getSharedSet(result)`, which interns the set in a
`ConcurrentHashMap`. The lookup returned the **wrong** interned canonical set
`{@Produces, @Parameters}` — so the producer's qualifiers became
`{@Produces, @Parameters}`, and `@Produces` was then validated as a qualifier →
`WELD-001301`.

Why the map matched the wrong key:

- Weld's interned sets are `org.jboss.weld.util.collections.ImmutableTinySet`
  (members in named `element1`/`element2` fields — **not** an array-backed layout).
- `ImmutableTinySet.equalsSet(other)` is `size==size && other.containsAll(this)`.
- So `{Produces,Parameters}.equals({Parameters,Any})` evaluates
  `HashSet{Parameters,Any}.containsAll(ImmutableTinySet{Produces,Parameters})`.
- CratonVM's `native_hs_contains_all` extracted the **foreign** argument via
  `collect_collection_elements`, whose layout heuristics don't model
  `ImmutableTinySet` → it returned an **empty** element vec → the containsAll loop
  never ran → **vacuously `true`**.
- That false positive made set equality asymmetric and non-reflexive, so the
  `ConcurrentHashMap` key comparison matched two unequal sets.

Minimal repro (no annotations, plain Strings):

```java
Set<String> hs  = new HashSet<>(List.of("a","b"));
Set<String> imm = org.jboss.weld.util.collections.ImmutableSet.of("a","c"); // Doubleton
hs.containsAll(imm);   // CratonVM(before)=true, HotSpot=false   <-- the bug
```

## Fix

`native-collections/src/lib.rs`: switch the HashSet bulk ops that consume a
*foreign* argument collection from `collect_collection_elements` to the existing
`collect_collection_elements_or_real` (which falls back to the collection's real
`toArray()` when the layout heuristics yield empty — exactly what
`native_hs_add_all` already did):

- `native_hs_contains_all` — vacuous-true was the WELD-001301 trigger.
- `native_hs_remove_all` — would have removed nothing for a foreign arg.
- `native_hs_retain_all` — would have emptied the set (kept nothing) for a foreign arg.

Strictly more correct; no opt-out flag (no behavior change for modeled collections —
they never hit the empty fallback). `cargo check` clean.

## Verification

- Minimal `CAProbe`: `HashSet.containsAll(WeldImmutableSet)` / `.equals` now match
  HotSpot (`false`, symmetric).
- CDI bootstrap: `BOOT OK` (was `WELD-001301`).
- `org.hibernate.orm.test.cdi.type.CdiSmokeTests` → **PASS** (was FAIL).
- 9/11 sampled `cdi.*` classes PASS == HotSpot (CdiSmokeTests, StandardCdiSupportTest,
  Valid/Delayed CdiSupportTest, all three Mixed-access tests, both Cdi*HostedConverterTest).
- Regression suite green (8/8, incl. a new `RCollections` case covering foreign-collection
  `containsAll`/`removeAll`/`retainAll` + Set-keyed-map lookup).

## Residual (separate bug, NOT this one)

The 2 `HibernateSearch*CdiSupportTest` classes still fail — now with a **different**
error and with the qualifiers correctly shown as `[@Any @Default]`:

```
WELD-001524: Unable to load proxy class for bean Managed Bean
[class ...TheSharedApplicationScopedBean] with qualifiers [@Any @Default]
```

This is Weld client-proxy (`@ApplicationScoped`) bytecode-generation, an
**independent** CratonVM gap — file/track separately.

## 2026-08-04 correction — "9/11 PASS" no longer reproduces (unrelated, earlier-in-bootstrap blocker)

A fresh 2026-08-04 residual run shows all 14 classes in this cluster (not
just the 2 `HibernateSearch*` classes already flagged as a residual above)
FAILing — but with `java.util.concurrent.RejectedExecutionException` out of
`WeldStartup.startInitialization` → `ConcurrentBeanDeployer.addClasses` →
`ForkJoinPool.invokeAll`, a failure that happens **before**
`BeanAttributesFactory.initQualifiers` (the code this doc's `containsAll`
fix touches) ever runs. This is not a regression of the `containsAll` fix —
`native_hs_contains_all` still uses `collect_collection_elements_or_real` —
it's a separate, later-introduced bug (`CRATONVM_REAL_FORKJOINPOOL` became
default-on in `16ec5d7ad`, 2026-07-30, exposing an uncovered
`ForkJoinPool.invokeAll` overload in the real-FJP bridge allow-list) that
now blocks bootstrap one step earlier, so this doc's fix is currently
unreachable/unverifiable by the suite rather than wrong. Full analysis:
[`docs/known-issues/hibernate/cdi-cluster-forkjoinpool-invokeall-rejectedexecution-20260804.md`](../../../known-issues/hibernate/cdi-cluster-forkjoinpool-invokeall-rejectedexecution-20260804.md).
Re-verify this doc's "9/11 PASS" claim once the `invokeAll` gap is fixed.
