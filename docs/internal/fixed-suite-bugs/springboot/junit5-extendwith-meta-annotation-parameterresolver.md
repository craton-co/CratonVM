# JUnit 5 `@ExtendWith` meta-annotation `ParameterResolver` — ⚠️ MISDIAGNOSED (does-not-reproduce)

**Status (corrected 2026-06-20):** ⚪ **NOT a meta-annotation bug — does-not-reproduce on current `dev`.**
The first version of this doc claimed JUnit's composed-`@ExtendWith` discovery was broken on CratonVM
(`No ParameterResolver registered for EntityManagerFactoryScope` when running the Hibernate JUnit suite).
On direct investigation that is **wrong**: the annotation machinery works correctly and the test passes.

## What was actually observed

A single early run of `org.hibernate.orm.test.actionqueue.JtaCustomAfterCompletionTest` via a JUnit
`LauncherFactory` (`_hibrepro/Run.java`) reported `No ParameterResolver registered for parameter
[EntityManagerFactoryScope]`. That was a **non-reproducing transient** captured under heavy concurrent-build
CPU/GC load early in the session.

## Why it's not a meta-annotation gap (verified)

`_hibrepro/AnnoProbe.java` reproduces JUnit's exact discovery path and the underlying reflection primitives;
on CratonVM (**both** the pre-merge `dev` binary and the merged-`dev` binary) every layer is byte-identical
to HotSpot:

| probe | HotSpot | CratonVM |
|---|---|---|
| `AnnotationSupport.findRepeatableAnnotations(testClass, ExtendWith.class)` | 10 | **10** ✓ |
| `@Jpa` type `getDeclaredAnnotations()` includes `@Extensions` container | yes | **yes** ✓ |
| `ExtendWith` `@Repeatable` container = `Extensions` | yes | **yes** ✓ |
| `Jpa.getAnnotation(Extensions).value().length` | 3 | **3** ✓ |
| `Jpa.getAnnotationsByType(ExtendWith.class).length` | 3 | **3** ✓ |
| `Jpa.getDeclaredAnnotationsByType(ExtendWith.class).length` | 3 | **3** ✓ |

(The direct + container `@Repeatable` merge — bug06-fam6 `0749b0dd` — and the meta-annotation recursion both
work.) `JtaCustomAfterCompletionTest` then **passes 5/5** via the JUnit launcher: pre-merge `REAL_NET`,
merged-`dev` `REAL_NET`, two merged-`dev` normal runs, and merged-`dev` **default (synthetic-socket) mode**
(`found=2 started=2 ok=2 failed=0` each).

## If it ever recurs

It would belong to the **open Family-A GC-root-under-JIT reflection race** (reflection arrays reclaimed under
GC during JUnit's reflection-heavy extension registration — see
[reflrepro-register-resident-jit-root-handoff.md](../internal/reflrepro-register-resident-jit-root-handoff.md)
and the Family-A table in [README.md](README.md)), **not** an annotation-synthesis bug. Attempts to force it
with `CRATONVM_DBG_GC_STRESS=65536/262144` only made the Hibernate bootstrap too slow to finish within the
timeout; they did not reproduce the `No ParameterResolver` signature. No separate, actionable defect here.
