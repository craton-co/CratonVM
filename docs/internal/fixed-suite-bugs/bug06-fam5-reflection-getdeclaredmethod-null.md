# BUG-06-FAM5 — reflection returns `null` where HotSpot returns a `Class`/`Method` (`getDeclaredMethod on null` ×28)

> **✅ CLOSED 2026-07-02 — failcause EXTINCT; the per-test-attribution re-run found ZERO instances.**
> The doc's "next step" (suite re-run with per-test attribution, then trace the owning frame) was
> executed and there is nothing left to attribute:
>
> 1. **Two clean full-coverage re-runs** of every non-passing suite class against frozen dev
>    binaries — dev `d707c97e` 2026-06-30 (2279 classes, jit-real, 6 shards,
>    `apps/spring-suite-runner/out/rerun-20260630-190102`) and dev `f7506e02` 2026-07-01
>    (759 classes, jit-real, `out/rerun-20260701-143455`) — contain **0 ×
>    `getDeclaredMethod on null`** and 0 × `currentContext on null` anywhere in
>    `raw.log`/`failcauses.log`. Other `Cannot invoke … is null` families DO still appear in the
>    same logs (`Map.get` ×10, `TypeAnnotation$LocationInfo.popLocation` ×5, …), so the capture
>    pipeline demonstrably records this failure shape; absence is real. (A class that passes
>    cannot carry the failcause, so non-passed coverage ⇒ suite-wide coverage.)
> 2. **Fresh targeted verification on dev `ffb247e5`** (2026-07-02, isolated-worktree build, frozen
>    binary `vmfam5close-0702.exe`): the 196-class fam5 surface (`core.annotation`, `core.type`,
>    `context.annotation`, `aop.framework`, `ResolvableType`/`GenericTypeResolver`/
>    `MethodParameter`/`BridgeMethodResolver`, `ReflectionUtils`/`ClassUtils`/`MethodInvoker`/
>    `TypeUtils`) run under **both** `--nojit` (this doc's mode) and JIT: identical results
>    164 OK / 24 FAIL / 6 EMPTY / 2 TIMEOUT, 2391/2489 test-methods passing, and **0 ×
>    `getDeclaredMethod`, 0 × `Cannot invoke` NPEs of any kind** in either mode
>    (`out/fam5close-nojit-real-all-20260702-170843`, `out/fam5closejit-jit-real-all-20260702-170847`).
>    The residual FAILs are the separately-tracked `context.annotation` scoped-proxy/AOT/JSR-330
>    cluster and fam6's `synthesizedAnnotationShouldReuseJdkProxyClass` — none reflection-null.
>    The `Refl5` probe remains byte-identical to HotSpot (JDK 25) in both modes on this binary.
>
> **Conclusion:** the ×28 was, as diagnosed, a cross-family **cascade** — its sources (fam1
> field-updaters `fe52db3a`, fam3 `findLoadedClass` `4b923e86`, fam4 synthetic-`Object` superclass
> `40b6d94a`, the `toArray` self-recursion `8795b88d`, bug-04 precise GC maps, bug-05 generics)
> have all been fixed on `dev`, and the aggregate died with them. The raw 2026-06-16 per-test data
> was never committed, so retroactive attribution is impossible — and with zero live instances,
> unnecessary. No CratonVM reflection native returns null where HotSpot returns a value on any
> path this census exercised.

> **UPDATE 2026-06-20:** Still not reproducible standalone. During the fam6 synthesis work
> (branch `fix/bug06-fam6-repeatable-merge`) the entire `AnnotationUtilsTests` (72/72) and
> `AnnotatedElementUtilsTests` (82/82) reflection surface passed under `--nojit`; no
> `getDeclaredMethod`-on-null surfaced in any annotation-cluster class. Consistent with the
> "cross-family cascade, needs suite-level bisection" diagnosis below — not a single reflection-null.

> **RETRY 2026-07-01:** Rebuilt the tracked standalone probe
> `docs/internal/fixed-suite-bugs/repros/bug06-fam5-reflection-null/Refl5.java` (lived under
> `docs/known-issues/repros/` until the 2026-07-02 close) with JDK 25 and ran it on
> HotSpot plus the current `dev` CratonVM binary
> (`C:\craton\CratonVM\target\release\cratonvm.exe`, timestamp 2026-07-01 01:30). The
> reflection output still matches exactly through `DONE`; CratonVM only appends its normal
> watchdog/VM-exit diagnostics. No standalone reproduction was recovered. Keep this open
> only for suite-level per-test attribution of the original `getDeclaredMethod`-on-null
> aggregate.

**Severity:** Medium — CV-unique reflection mismatch in the Spring suite assertion tail.
**Status:** ✅ **RESOLVED / EXTINCT** (2026-07-02, see CLOSED banner above). Previous status 🟡 PARTIAL (audit 2026-06-19): the one clean family-5 reflection-null was **FIXED** — lambda/method-ref `getGenericSuperclass` now returns `Object` not `null` (`9d0974cf`, default path; `lang_class.rs` lambda guard); the headline `getDeclaredMethod`-on-null ×28 was a cross-family **cascade** (bug-04 GC + bug-05 generics + synthetic-type gaps), not a single reflection-null, and died with its sources. Refl5/Refl6/BridgeProbe/SpringFam5 are byte-identical to HotSpot.
**Mode:** Interpreter (JIT-off); the null comes from a native, not codegen.
**HotSpot (JDK 25):** the affected reflection calls return non-null.
**Origin:** family 5 of the bug-06 assertion-mismatch census (`spring-suite/crash-reports-2026-06-16/bug-06-assertion-mismatch-families.md`).

## Symptom

JEP-358 helpful-NPEs in the Spring suite, dominated by:

```
Cannot invoke "java.lang.Class.getDeclaredMethod(...)" because "<x>" is null   ×28
Cannot invoke ... currentContext() ... because "<x>" is null                  ×15   (Reactor — downstream of family 1, NOT this bug)
```

The `currentContext on null` ×15 is Reactor `Context` and is a **cascade of family 1**
(field-updater `NoSuchMethodError` already fixed `fe52db3a`); it is **not** part of this
family. The CV-unique core here is `getDeclaredMethod on null` (×28): a **native reflection
method returned Java `null`** where HotSpot returns a `Class`/`Method`, and the caller then
dereferences it.

## Refuted hypothesis (do not re-try)

The original agent root-cause — "`synthetic_class_mirror` writes `Object(None)` into mirror
slot 0, so some reflective lookup reads null" — is **wrong**:

- `lang_class.rs:660-661` documents that `Object(None)` in slot 0 is **intentional** in
  real-JDK mode (it backs the `isArray`/primitive-mirror contract).
- `synthetic_class_mirror` always returns a **non-null** mirror; it cannot be the source of a
  null receiver.

**⚠️ Do NOT touch `synthetic_class_mirror` slot 0 — it would break the documented
`isArray`/primitive-mirror contract without addressing this bug.**

## Verified clean (2026-06-18, dev binary, `--java-home` JDK25)

Probe `spring-suite/probe/Refl5.java` — 29 cases over the common reflection surface
(`getSuperclass`, `getComponentType`, `getDeclaringClass`, `getEnclosingClass`,
`Method.getDeclaringClass`, `Class.forName`) — is **byte-for-byte identical to HotSpot**.
So the ordinary reflection paths are correct; the null comes from a **narrower path** — a
generic/proxy/synthetic-type or member-lookup variant not covered by the 29 cases.

## Why it isn't pinned yet

The bug-06 suite census (`FAIL-ANALYSIS.md`) aggregates the `getDeclaredMethod on null`
failcause **across tests** and does **not** attribute it to a specific test class, so there is
no single failing stack to trace. The reflection method that returns null (and the exact
receiver type that triggers it) is unknown.

## Next step

1. Re-run the Spring suite with **per-test attribution** for the `getDeclaredMethod on null`
   failcause (capture the owning test class + the precise frame) — now easier with the
   stack-trace-order fix (`c3867a4e`) landed.
2. From the captured frame, identify which `Class`/`Method` reflective accessor returned null
   and the receiver type (likely a generic/proxy/synthetic class), then add that exact case to
   `Refl5.java` to reproduce standalone.
3. Fix the native to return the HotSpot value for that type; re-run `Refl5` + the owning test.

## Related

- Family 1 (field-updaters) — the `currentContext on null` cascade source, already fixed (`fe52db3a`).
- Family 3 (`findLoadedClass`) — fixed/merged (`18707ee3`); a sibling "native returns wrong
  value vs HotSpot" reflection-surface bug, settled the same way (reproducer-vs-HotSpot diff).
