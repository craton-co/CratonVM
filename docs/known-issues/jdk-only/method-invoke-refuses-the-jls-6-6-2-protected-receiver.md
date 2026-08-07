# `Method.invoke` refuses the JLS 6.6.2 protected-receiver allowance

**Status:** OPEN, filed 2026-08-07 from the strict-corpus re-measurement. **Not a
`--jdk-only` defect** — it reproduces identically in `--real-jdk`, and it is one
of the two mode-independent failures left in the corpus.

| | |
|---|---|
| **Vector** | `regression-suite/src/RJdkFieldModule.java:1123` (`methodInvokeReceiverRefinement`) |
| **Symptom** | `AssertionError: Object.finalize through the caller's own class: expected OK but got IllegalAccessException` |
| **Modes** | `--jdk-only` **and** `--real-jdk`, byte-identical |
| **Direction** | **over-refusal** — legitimate access denied |

## What the spec says

JLS §6.6.2: a `protected` member declared in another package is accessible to a
subclass *only* through a receiver whose type is the accessing class or a
subclass of it. `java.lang.reflect.Method.invoke` enforces the same rule
(`AccessibleObject`'s access check, `Reflection.verifyMemberAccess` plus the
`targetClass` refinement) — so a class `C` may reflectively invoke
`Object.finalize` on a `C` or a subclass of `C`, and on nothing else.

The refinement is the part CratonVM is missing: the check is not "may I touch
this member" alone, it is "may I touch it **through this receiver**".

## Measured

Reduced to a self-contained probe, no modules and no `setAccessible` — the call
under test is a plain `Method.invoke`. Run each arm with `--java-home` pointed at
the JDK 25 image; a hand-run without it measures the host's default JDK.

```java
static class Finalizes {
    static class Deeper extends Finalizes {}
    static String invoke(Object receiver) {
        try {
            Object.class.getDeclaredMethod("finalize").invoke(receiver);
            return "OK";
        } catch (Throwable t) { return t.getClass().getSimpleName(); }
    }
}
```

| receiver | HotSpot 25.0.3 | CratonVM `--real-jdk` | CratonVM `--jdk-only` |
|---|---|---|---|
| `new Finalizes()` — the caller's own class | **OK** | `IllegalAccessException` | `IllegalAccessException` |
| `new Finalizes.Deeper()` — a subclass of the caller | **OK** | `IllegalAccessException` | `IllegalAccessException` |
| `new Object()` — the declaring class | `IllegalAccessException` | `IllegalAccessException` | `IllegalAccessException` |
| `"x"` — unrelated | `IllegalAccessException` | `IllegalAccessException` | `IllegalAccessException` |

The two DENY rows already agree. Only the ALLOW rows are wrong, which is the
signature of a check that stops at "is this member protected and declared
elsewhere?" and never consults the receiver's class.

**A suite that only asserted the denials would pass.** Both negative cases are
already correct, and an implementation that refuses *everything* satisfies them
both. `RJdkFieldModule` catches it only because it asserts the positive half as
well.

## Why this is worth fixing beyond the vector

`Object.finalize` is the test's probe, not the point. The rule governs every
reflective call to an inherited `protected` member — `Object.clone` is the one
real code actually reaches, and a library that reflectively clones through a
subclass receiver gets an `IllegalAccessException` where every other JVM
succeeds. Over-refusals are worse than the equivalent over-permission here,
because they surface as a hard failure in code that is correct.

## Where to look

The receiver refinement belongs next to the existing member-access check, not in
a new gate — the deny cases prove the surrounding plumbing already works. Note
`setAccessible(true)` is a *different* path and is not implicated: with it the
call is refused by module encapsulation on HotSpot too
(`InaccessibleObjectException`, all four receivers), so any fix must not be
validated through a probe that sets it.

Related: [`W6-8-method-invoke-exports-gate.md`](W6-8-method-invoke-exports-gate.md)
(the exports half of the same check),
[`L1-reflect-setaccessible-invoke.md`](L1-reflect-setaccessible-invoke.md),
[`L15-nestmate-access-field-and-constructor.md`](L15-nestmate-access-field-and-constructor.md),
and the campaign record
[`STRICT-CORPUS-CAMPAIGN-20260807.md`](../../feature-designs/jdk-only-wave2/STRICT-CORPUS-CAMPAIGN-20260807.md).
