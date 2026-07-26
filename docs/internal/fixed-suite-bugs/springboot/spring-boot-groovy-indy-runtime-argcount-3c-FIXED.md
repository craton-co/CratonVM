---
name: spring-boot-groovy-indy-runtime-argcount-3c-FIXED
description: FIXED. The SpringRepos layer-3c Groovy invokedynamic AIOOBE (IndyGuardsFiltersAndSignatures.sameClasses via IndyInterface.fromCache) plus the void-target poly-invoke underflow (3d) it revealed. Both fixed on branch fix/springrepos-indy-3c in native-builtins/src/lang_invoke.rs.
metadata:
  type: known-issue-fixed
  area: invoke, groovy, indy
---

# SpringRepos layer 3c — Groovy indy `sameClasses` AIOOBE — FIXED (+ 3d void underflow)

**Status:** ✅ FIXED on branch `fix/springrepos-indy-3c`
(commit `5d36c432`, `native-builtins/src/lang_invoke.rs`). Two coupled defects.
Parent cascade: [[springrepos-extension-hang-jit-throughput-and-deep-recursion]].
Standalone repros: `../repros/springrepos-indy-3c/`.

## 3c — `guardWithTest` dropped the receiver from the adapter `type()`

`MethodHandles.guardWithTest(test, target, fallback)` (`mhs_guard_with_test`)
built the GUARD adapter's descriptor from the target's **raw** bytecode
descriptor (`mh_read_desc`). For an unbound virtual/special target that
descriptor omits the leading receiver (CratonVM stores the raw `MH_DESC` without
the receiver and only prepends it in the effective `type()` — layer 3). So the
GUARD adapter's `type()` had **one fewer parameter** than the target.

Groovy's `Selector.setGuards` (decompiled, groovy-4.0.29) builds the bulk guard:

```java
Class<?>[] classes = Arrays.stream(args).map(o -> o==null?null:o.getClass()).toArray(...); // len = args.length
Class<?>[] pa = handle.type().parameterArray();                                            // <-- shrunken in CV
handle = guardWithTest(
    SAME_CLASSES.bindTo(classes).asCollector(Object[].class, pa.length).asType(methodType(boolean, pa)),
    handle, fallback);
```

`SAME_CLASSES.sameClasses(Class[] cs, Object[] os)` loops `for i in 0..cs.length`
reading `os[i]`. With `cs.length = args.length` but the collector sized to the
shrunken `pa.length`, `os` was shorter than `cs` → AIOOBE in
`IndyInterface.fromCache` (`getCachedMethodHandle().invokeExact(args)`). Because
`setGuards` reads `pa` *after* earlier `guardWithTest` layers (SAME_MC,
switchpoint) already shrank the count, the discrepancy compounded.

**Fix:** chain off the EFFECTIVE type (`mh_type_descriptor`, the same helper
layers 3/3b use), not the raw `MH_DESC`. `handle.type()` pc 1→2, == HotSpot.
`SwitchPoint.guardWithTest` delegates to `MethodHandles.guardWithTest`, so it is
covered by the same fix.

## 3d — Object-returning poly-invoke of a `void` target underflowed (revealed by 3c)

Once 3c was fixed, dispatch reached `fromCache`'s
`cachedMethodHandle.invokeExact(args)` and hit
`operand-stack underflow at value return ... IndyInterface.fromCache pc=142`.

Groovy call sites always return `Object`, but the resolved method
(`addRepositories(Closure)`) is `void`. The guarded handle's effective return
type is `Object` (`L`), yet `mh_dispatch` of the void leaf returns `Ok(None)`.
`auto_box_return`'s reference-return arm (`_ => result`) passed `Ok(None)`
through, so `invokeExact` pushed **nothing** and `fromCache`'s `areturn`
underflowed (VM-fatal).

**Fix:** `auto_box_return` maps `Ok(None)` → `Ok(Some(Value::Object(None)))` for
`L`/`[` returns. HotSpot bakes a void→null filter into the `asType(...→Object)`
adapter; CratonVM's `asType` is a passthrough, so we normalize at the poly-invoke
boundary. Thrown exceptions still propagate.

## Result & what remains

`SpringRepositoriesExtensionTests`: **0/11 (all crashed) → 3/11** (clean, no
crashes; the VM exits normally). The 3 passing are the `isEmpty()` cases.

The remaining **8** failures are a SEPARATE, deeper layer (filed as
`spring-boot-groovy-indy-mockito-mock-dispatch.md`): `this.repositories.maven {
… }` is dispatched on a **Mockito mock** via Groovy indy, but the stubbed
`maven(any(Closure.class))` answer is never driven (the repositories list stays
empty), so every "expected size N" assertion fails. This is Groovy MOP +
Mockito/ByteBuddy interception (or `maven(Closure)` vs `maven(Action)` overload
selection), not an indy arity/return-value issue.

## Validation
- 12/12 `lang_invoke` unit tests; 22/23 `wp2_2_method_invoke_matrix` (the 1 fail
  = pre-existing `Method.getParameterCount` essential-registration expectation,
  untouched by this change); `wave2_c_methodhandles` green.
- Probes `LayeredGuardProbe` / `VoidTargetProbe` / `CachedGuardProbe` ==
  HotSpot (docs/internal/repros/springrepos-indy-3c/).
- Default-on, no gate (these are correctness fixes to existing combinator paths).
