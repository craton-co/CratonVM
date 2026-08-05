# `CacheAutoConfigurationTests`: config-class parse fails with `Object.accept` NoSuchMethodError, alongside a GC reclaimed-memory guard fault and a bytecode-verification stack overflow

**Status: OPEN — found 2026-08-05**

## Symptom

Not the already-FIXED `cacheautoconfigurationtests-infinispan-null-cachemanager-FIXED.md`
(that was `getCacheNames()` returning null → `AssertionError`; this is a
different failure entirely, at config-class parsing time).

### 1. `NoSuchMethodError: java.lang.Object.accept(...)` while parsing `@Configuration` classes

```
BeanDefinitionStoreException: Failed to parse configuration class [CacheAutoConfiguration]
Caused by: java.lang.NoSuchMethodError: java.lang.Object.accept(Ljava/lang/Object;Ljava/lang/Object;)V
```
i.e. a `BiConsumer`-shaped lambda/functional-interface call dispatched
against the erased `java.lang.Object` receiver rather than the real
synthetic lambda class — same general "dispatched against erased Object"
*shape* previously seen and fixed for method references
(`healthendpointgroup-methodref-dispatch-nosuchmethod-20260728-FIXED.md`,
now retired as non-reproducing), but that doc covered
`HealthEndpointGroup::getAdditionalPath`, an unbound instance-method
reference — a functionally distinct shape from whatever produces
`Object.accept` here.

### 2. GC guard fault immediately preceding it, same run

```
ERROR cratonvm::gc::guard: receiver points into RECLAIMED memory — a still-referenced object was
  collected. `java.lang.Object` here is the all-zero header the collector left behind, not a real
  Object. obj=0x2002da56e08 site="invoke dispatch" ... target_class=java/lang/Object.accept(...)
ERROR cratonvm::gc::guard: receiver is inside a YOUNG span the non-moving sweep zeroed and returned
  to the free list. ...
```
Both errors name the exact same `accept` call site and target. This strongly
suggests the `NoSuchMethodError` above is a *symptom*, not the root cause: a
live lambda/proxy object backing this `BiConsumer` call was collected while
still reachable (or its root wasn't tracked across a JIT frame — the log
also shows repeated `[moving-young] fallback` warnings for
"innermost-rbp-belongs-to-unguarded-callee" / "unregistered-jit-frame-on-stack"
immediately before), so the receiver reads back as a zeroed header, and the
zeroed/reclaimed object naturally has no methods — `Object.accept` is what a
freed span looks like from the interpreter's point of view, not a real
dispatch bug.

### 3. Separate: bytecode-verification stack overflow on retransform

```
WARN retransformClasses0: UnsupportedClassRedefinitionError { class_name: "org/infinispan/configuration/cache/ConfigurationBuilder",
  message: "new bytes failed bytecode verification: verification error in
  org/infinispan/configuration/cache/ConfigurationBuilder.simpleCache: at bytecode offset 27: stack overflow during verification" }
```
Later in the same run, `configurationBuilder` bean creation fails via a
distinct path (verifier rejects a Mockito/ByteBuddy-redefined
`ConfigurationBuilder`). This looks unrelated to items 1-2; not
investigated further given time budget.

## Root cause

Not confirmed. Item 2 (GC reclaiming a live object referenced by a JIT
frame the collector's root-scan can't fully prove) is the most promising
lead and matches this codebase's known "moving-young fallback" root-map
gap — worth checking whether the fallback path (non-moving/free-list sweep)
has its own separate liveness bug distinct from the moving path, since the
guard fault fires specifically during a fallback-mode collection.

## Where to look next

`gc_quiescence`'s fallback reasons (`innermost-rbp-belongs-to-unguarded-callee`,
`unregistered-jit-frame-on-stack`) and whether the non-moving/free-list
sweep's liveness marking correctly retains an object whose only live
reference is inside a JIT frame it couldn't build a root map for — this is
exactly the failure mode the fallback exists to be conservative about, so if
it's reclaiming those objects instead of unconditionally retaining them,
that's the bug. `grep -rn "innermost-rbp-belongs-to-unguarded-callee\|non-moving" vm/ --include=*.rs`.

## Affected classes

- `module/spring-boot-cache` — `org.springframework.boot.cache.autoconfigure.CacheAutoConfigurationTests`
