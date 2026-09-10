# Lane 3 — core reflection and `java.lang.invoke`

**Scope: 251 §1.4 shadows over 36 classes, from 246 registration sites.**
Prefixes: `java/lang/reflect/`, `jdk/internal/reflect/`, `sun/reflect/`,
`java/lang/invoke/`.

Read [`lane-0-integration-and-gates.md`](lane-0-integration-and-gates.md) §2-§6 first. Method, preconditions and
landing protocol: [`../jdk-only-lane-operations.md`](../../contributing/jdk-only-lane-operations.md).

---

> **Lane T closed 2026-09-10.** Its throwable-family rows are RETIRED
> (`RETIRED_SHADOW_LT_TRIPLES`, 906 triples over 62 classes), so a triple this
> page defers to lane T is either already retired or classified as blocked —
> check the table before treating it as unowned. Record: [the lane T record](../../internal/jdk-only/lane-t-the-throwable-family-retired-and-the-three-defects-the-arm-had-to-find-first-20260910.md).

## 1. Shape of the lane

```text
  35  java/lang/reflect/Field                 11  java/lang/invoke/MethodHandle
  33  java/lang/reflect/Method                11  sun/reflect/generics/...
  25  java/lang/invoke/MethodHandles$Lookup        TypeVariableImpl
  23  java/lang/invoke/MethodHandles           9  java/lang/invoke/MethodType
  23  java/lang/reflect/Constructor            7  java/lang/invoke/MemberName
  14  java/lang/reflect/InaccessibleObjectException  <- lane T
  12  java/lang/reflect/InvocationTargetException    <- lane T
```

Almost one registration site per row: this lane is a long tail of hand-written
registrations, not a few parameterised loops. That makes waves smaller and
individually cheaper than L1's or L4's, and it means the source-scanning drift
gate can actually see your work — unusually for this campaign.

`InaccessibleObjectException` and `InvocationTargetException` are lane T's
throwable registrar. Not yours.

## 2. Your first dependency is already satisfied — check it

Reflection lookups compare **binary** names. `java/lang/Class.getName` was
returning the internal slash form whenever the native yielded
(`java/lang/Object` instead of `java.lang.Object`), silently, and it propagated
into every JDK name comparison. It is now a reviewed `Intrinsic` (L0 §7), which
took `ClassNameSweep` from 24 diffs of 24 to 2.

**Re-confirm this on your tree before diagnosing any name-shaped failure here**,
and if you see a slash in a name, suspect a *different* accessor rather than
re-deriving the same finding.

The residual worth knowing: with the shadow dial armed,
`Class.forName(Nested.class.getName())` still throws
`ClassNotFoundException: ClassNameSweep$Nested`. That is `forName0` declining at
a dispatch door — and `forName0` **is** `ACC_NATIVE` in the image, so contract
§1.5 makes `Bridge` correct for it. It is a dial artefact, not a retirement
target.

## 3. The three instrument traps that have already burned this area

These are recorded findings, not hypotheticals. Every one produced a wrong
conclusion first.

- **`getDeclaredFields` on `java.lang.reflect.*` returns 0 on a healthy image.**
  Core reflection deliberately hides its own fields. A reflective field walk
  across this package reports ABSENT at every link on a VM that is working
  perfectly. Do not use a field walk as your instrument here.
- **A skip list can hide the real caller.** `ReflectionFactory` calls
  `setAccessible` from a package the caller-sensitive skip list hides, so the
  apparent caller is not the real one. **Print the throw stack first**, before
  theorising about which access check fired.
- **A bare exception-type assertion cannot say which check fired.**
  `InaccessibleObjectException` has several distinct sources; assert on the
  message or the frame, not the type.

## 4. `MemberName`, `MethodHandle`, `MethodType`: expect reviewed `Intrinsic`s

`java/lang/invoke/MemberName` (7) and `MethodHandle` (11) are the most
VM-coupled rows in the campaign after `Class`. `MemberName` in particular
carries fields (`clazz`, `name`, `type`, `flags`, `method`, `resolution`) that a
real JVM fills during `MethodHandleNatives.resolve`. Where a field is written
only by a VM and this VM's layout differs, §1.4's remedy returns null or a wrong
value — which is the reviewed-`Intrinsic` case, not a retirement.

Follow L0 §7's protocol exactly, and note the cost it names: an `Intrinsic` is
exempt at every dispatch door **and** exempt from the census by construction, so
tagging removes the row from the population the dial can ever ask about. Report
all three numbers (unarmed / yielded / tagged) or leave it a `Bridge`.

`MethodHandles$Lookup` (25) and `MethodHandles` (23) are different: much of
their surface is ordinary Java over the JDK's own checks, so they are plausible
straight retirements. Probe the *access-control* answers — `lookupModes`,
`privateLookupIn`, a cross-module `findStatic`, `unreflect` on a
non-accessible member — because that is what the JDK's bytecode gets right and a
hand-written native tends to approximate.

## 5. `Field`/`Method`/`Constructor` — 91 rows, one shared story

They share `AccessibleObject` (bucket B) and a copy-on-`getDeclaredX` model:
the JDK hands out *copies* with a shared root, and `setAccessible` writes the
`override` flag on the copy. Two probe requirements follow:

- **Ask the same object twice.** `getDeclaredField("x") == getDeclaredField("x")`
  is `false` on HotSpot, and `.equals` is `true`. A retirement that changes
  copying changes both answers; print both.
- **`setAccessible` then read, on a fresh copy.** The flag must not leak between
  copies, and must survive on the one you set it on.

Also print `getGenericType`/`getGenericParameterTypes` for at least one generic
member: `sun/reflect/generics/reflectiveObjects/TypeVariableImpl` (11 rows) is
reached only through those paths, and it is otherwise untested territory.

## 6. Coordination

- **L0** owns `java/lang/Class`. Most reflection entry points route through it;
  if a failure resolves to a `Class` row, hand it to L0 rather than tagging it.
- **L7** owns the loader and its bootstrap failures. `setAccessible` and module
  access answers depend on module state that L7 may still be repairing —
  re-price after any L7 landing.
- **Lane T** owns the two throwable classes in your prefix.

## 7. The increment loop

1. Funnel from a dump: owns slot, kind `Bridge`, image `Code`, `invocations > 0`
   in **your** instrument's run.
2. Probe + HotSpot oracle. No build needed.
3. Fill `RETIRED_SHADOW_L3_TRIPLES`, sorted and unique. Note that
   `java/lang/reflect/`, `java/lang/invoke/` and `sun/reflect/` must be present
   in `RETIRED_SHADOW_PREFIXES` — L0's skeleton commit adds them; an entry
   outside every prefix silently answers "not retired" and is invisible in a
   workload.
4. Build token (L0 §5); one build per wave.
5. `N refusals, 0 survivors`.
6. Probe-tree A/B, `--jdk-only` corpus, `SUITE=all` at `TIMEOUT=600`, `all`-arm
   count.
7. Full gate set. Kind-map rows. Commit. Do not push.

## 8. Done

Every bucket-A/B row in the prefix set is retired, classified as C/D/E/F, a
reviewed `Intrinsic` with its probe, or blocked with the blocker named — with
the `invoke` package's VM-coupled rows explicitly separated from the ones that
were genuinely retirable.
