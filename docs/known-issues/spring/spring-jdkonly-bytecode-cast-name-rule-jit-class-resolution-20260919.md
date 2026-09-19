# `--jdk-only`: bytecode `instanceof` / `checkcast` still accept a same-named class of another loader, because the JIT can hand a forked loader the application's copy

| | |
|---|---|
| **Status** | **OPEN**. The reflective half (`Class.isInstance`/`isAssignableFrom`/`cast`/`asSubclass`) is fixed: `docs/internal/fixed-suite-bugs/spring/spring-jdkonly-forked-loader-assignability-FIXED-20260919.md`. This page owns the bytecode half, which was tried and reverted. |
| **Scope** | `--jdk-only`, JIT on (the default). Interpreter-only (`CRATONVM_DISABLE_JIT=1`) is already faithful. |
| **Impact** | None measured on a passing Spring class: no test fails because `instanceof` answers true across loaders. It is a divergence from HotSpot (`SpringJdkOnlyForkedLoaderProbe` had one row for it, removed while this is open). |

## The divergence

```java
Object o = forkDefinedInstance;      // class defined by a loader that redefined p.Bundle
o instanceof Bundle                  // HotSpot: false      --jdk-only: true   (Bundle = the application's)
```

`op_instanceof` and `op_checkcast` (`vm/src/runtime/interpreter/opcodes.rs`) end their
assignability chain in `loader_aware_name_assignable` (`typecheck.rs`), a binary-name walk
over the receiver's superclass / interface chain, and the JIT's loader-duplication
fallbacks in `vm/src/jit/helpers.rs` (`jit_typecheck_resolve`, two of them) do the same.
It is the same rule `Class.isInstance` had.

## What was tried

Both interpreter sites and the JIT's first fallback gated on
`!shared.config.is_jdk_only()` (three edits, each a two-line change). The probe row went
green. The 733-class subset of the Spring suite (aot, annotation, `core.type`, cglib, aop,
proxy, groovy, class-loader and SpEL classes) then had **new** failures that pass without
the gate:

| Class | Cause |
|---|---|
| `PersistenceAnnotationBeanPostProcessorAotContributionTests.processAheadOfTimeWhenCustomPersistenceUnitOnPublicSetter` | traced, below |
| `AotIntegrationTests.endToEndTests`, `endToEndTestsForBeanOverrides` | `ClassCastException: org.assertj.core.api.StringAssert cannot be cast to org.assertj.core.api.AbstractStringAssert` |
| `ConfigurationClassPostProcessorAotContributionTests` (2), `InitDestroyMethodLifecycleTests` (2), `ImportHttpServiceRegistrarTests` (2) | `ExceptionInInitializerError` (not diagnosed) |
| `TestContextAotGeneratorIntegrationTests` (all 4: `Failed to process test class [...Vintage...] for AOT`), `ReactiveTypeHandlerTests` (LOADERR, `junit-jupiter` failed to discover tests) | not diagnosed (the runner's batch mode; not re-run alone with the gate) |

## The one that was traced

`PersistenceAnnotationBeanPostProcessorAotContributionTests...OnPublicSetter`
(`Mockito cannot mock this class: BeanRegistrationCode`, underlying `ClassCastException:
JavaDispatcher$Dispatcher$ForInstanceCheck cannot be cast to JavaDispatcher$Dispatcher`).
A trace at the failing `checkcast` inside `ByteBuddy`'s
`JavaDispatcher$ProxiedInvocationHandler.invoke`:

```
obj    = JavaDispatcher$Dispatcher$ForInstanceCheck   loader=UserDefined(3)   (the fork)
         its interface Dispatcher                       loader=UserDefined(3)   (the fork)
frame  = JavaDispatcher$ProxiedInvocationHandler        loader=Application     (id 4125, high: defined late)
```

Every ByteBuddy class the test touches belongs to the fork, except this one handler, which
is the **application's** copy. Something running in fork code instantiated the
application's `ProxiedInvocationHandler`. It passes with `CRATONVM_DISABLE_JIT=1` and with
the interpreter gate reverted, so the site is JIT-compiled fork code.

The mechanism, read from the code (the instantiating site itself was not pinned):
`resolve_jit_new_site` (`vm/src/runtime/interpreter/jit_bridge.rs`) resolves a `new`
site's class at COMPILE time through `ClassManager::find_class_by_name_for_class`. That
lookup sees only classes that are already loaded, and for a user-defined requesting loader
it answers "its own namespace, then the built-in chain" -- so a class the fork has not
defined yet resolves to the application's copy, and the JIT bakes that `ClassId` into the
compiled code. The interpreter takes the loader-faithful road (`resolve_class_loader_aware`
drives the fork's own `loadClass`). The name rule in `instanceof`/`checkcast` was hiding
exactly this: an application-copy object reaching fork code passed the cast by name.

## What a fix needs

1. The JIT's compile-time resolvers must not answer with a class the requesting user loader
   has not defined. `resolve_jit_new_site` can return `JitNewSite::Deferred` (the
   CP-indexed helper `jit_new_object_cp` resolves at run time through
   `resolve_class_loader_aware`); the typecheck-target intern
   (`intern_typecheck_target`) and the invoke / static-field owner resolution use the same
   lookup and need the same rule. The cost is confined to methods of classes a user loader
   defined.
2. Then re-apply the three gates (the interpreter's two `loader_aware_name_assignable`
   calls, the JIT's first `is_assignable_to_name`) under `is_jdk_only()`, re-add the
   `instanceof` rows to `tools/probes/SpringJdkOnlyForkedLoaderProbe.java`
   (`fork instance instanceof app Bundle` = false), and run the subset above with the JIT
   ON. `AotIntegrationTests` needs about 860 s.

Reproduce the traced case with the gate applied: `KM` on `spring-orm` with
`org.springframework.orm.jpa.support.PersistenceAnnotationBeanPostProcessorAotContributionTests`
`#processAheadOfTimeWhenCustomPersistenceUnitOnPublicSetter()`.
