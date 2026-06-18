# Bug 01 — JIT miscompile hangs JUnit5 discovery (whole suite blocked, JIT on)

**Severity:** Critical — with JIT enabled (the default), CratonVM hangs during
JUnit5 test *discovery* for **every** kafka-clients package. The 120 s default
watchdog fires and aborts. HotSpot discovers + runs the same package in well
under a second.

**Repro (smallest package):**
```
cratonvm.exe -Xmx2g -cp "<ktest>;<kafka-test-classes>;<lib/*>" RunPkg org.apache.kafka.common.serialization
```
- CratonVM (JIT on, default): hang → `T19.H1 watchdog: deadline of 120s elapsed` → abort.
- CratonVM `--nojit`: completes in ~8 s (66 tests, 1 unrelated failure).
- HotSpot: `RESULT ... started=66 failures=0 ms=624`.

## Root cause (bisected)

The hang is a JIT codegen miscompile, isolated with the in-tree bisection hooks:

1. `CRATONVM_JIT_BISECT_ONLY=java/,jdk/,sun/` → completes. Culprit not in the JDK.
2. `CRATONVM_JIT_BISECT_ONLY=org/junit/` → hang. Culprit in JUnit framework code.
3. Narrowed: `org/junit/platform/engine/support/discovery/` → hang.
4. Method-level bisection with `CRATONVM_JIT_BISECT_SKIP` over the 12 compiled
   methods in that package pinned a **single** culprit:

```
org/junit/platform/engine/support/discovery/EngineDiscoveryRequestResolution.lambda$resolve$2
  (Lorg/junit/platform/engine/DiscoverySelector;Lorg/junit/platform/engine/support/discovery/SelectorResolver;)
   Lorg/junit/platform/engine/support/discovery/SelectorResolver$Resolution;
```

Skipping just this one method (`CRATONVM_JIT_BISECT_SKIP=...EngineDiscoveryRequestResolution.lambda$resolve$2`)
makes discovery complete; skipping the other 11 does not.

The method is a 13-way `instanceof`/`checkcast`/`invokeinterface` dispatch chain
over the overloaded `SelectorResolver.resolve(<SelectorType>, Context)` methods
(`PackageSelector` is the 13th arm). When JIT-compiled it returns a wrong
`Resolution`, so the discovery work-queue in `EngineDiscoveryRequestResolution.resolve`
never drains — an infinite loop (confirmed: hot interpreted callees
`Preconditions.notNull`/`condition`/`Integer.compare` climb past 10 000 invocations;
the loop is a genuine logic spin, **not** GC thrash — `-Xmx8g` does not help and
there are **zero** GC corruption warnings during the hang).

Minimal Java reproductions of the dispatch shape (int return; reference return +
stream `.map` lambda; interface default methods; 4 sibling subclasses) all run
**correctly** on CratonVM JIT-on, so the trigger depends on a subtle property of
the real method (size/register pressure/OSR entry) not yet captured in isolation.

## Partial fix from dev `3ccd0bef` + status update (2026-06-12)

After merging dev's `fix(reflection,dispatch): … don't retarget static iface
method refs` (`3ccd0bef`) — which is exactly the invokeinterface-dispatch area
exercised by `lambda$resolve$2`'s overloaded `SelectorResolver.resolve(...)` chain
— the hang **no longer reproduces for `common.serialization`** with JIT on (it now
completes). So `3ccd0bef` fixes at least the serialization trigger.

A provisional JIT skip-list ban on the discovery package was tried and **removed**:
post-merge it was net-negative (serialization *hung* with the ban active and
*completed* with it lifted), so shipping it would have regressed the dev fix.

**Still open:** with default JIT, some packages still hang in discovery
(`org.apache.kafka.common.config` reproduces). So the codegen issue is not fully
resolved — there remain trigger(s) beyond the one `3ccd0bef` addressed. The whole
suite still runs cleanly under `--nojit`.

## Status
- [x] Root-caused to a single method (`lambda$resolve$2`) via bisection.
- [x] dev `3ccd0bef` fixes the serialization trigger (JIT-on now completes there).
- [ ] Remaining JIT-on discovery hang for other packages (e.g. `common.config`) — open.
