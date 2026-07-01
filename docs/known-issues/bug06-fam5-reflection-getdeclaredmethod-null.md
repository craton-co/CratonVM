# BUG-06-FAM5 — reflection returns `null` where HotSpot returns a `Class`/`Method` (`getDeclaredMethod on null` ×28)

> **UPDATE 2026-06-20:** Still not reproducible standalone. During the fam6 synthesis work
> (branch `fix/bug06-fam6-repeatable-merge`) the entire `AnnotationUtilsTests` (72/72) and
> `AnnotatedElementUtilsTests` (82/82) reflection surface passed under `--nojit`; no
> `getDeclaredMethod`-on-null surfaced in any annotation-cluster class. Consistent with the
> "cross-family cascade, needs suite-level bisection" diagnosis below — not a single reflection-null.

> **RETRY 2026-07-01:** Rebuilt the tracked standalone probe
> `docs/known-issues/repros/bug06-fam5-reflection-null/Refl5.java` with JDK 25 and ran it on
> HotSpot plus the current `dev` CratonVM binary
> (`C:\craton\CratonVM\target\release\cratonvm.exe`, timestamp 2026-07-01 01:30). The
> reflection output still matches exactly through `DONE`; CratonVM only appends its normal
> watchdog/VM-exit diagnostics. No standalone reproduction was recovered. Keep this open
> only for suite-level per-test attribution of the original `getDeclaredMethod`-on-null
> aggregate.

**Severity:** Medium — CV-unique reflection mismatch in the Spring suite assertion tail.
**Status:** 🟡 PARTIAL (audit 2026-06-19) — the one clean family-5 reflection-null is **FIXED**: lambda/method-ref `getGenericSuperclass` now returns `Object` not `null` (`9d0974cf`, default path; `lang_class.rs` lambda guard). Residual **OPEN**: the headline `getDeclaredMethod`-on-null ×28 is a cross-family **cascade** (bug-04 GC + bug-05 generics + synthetic-type gaps), not a single reflection-null; per-test attribution never completed. Refl5/Refl6/BridgeProbe/SpringFam5 are byte-identical to HotSpot. Handoff / needs suite-level bisection.
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
