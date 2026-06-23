# SBR-03 — `Object[] instanceof I[]` returns `true` (HotSpot: `false`)

**Status:** ✅ **FIXED** (worktree `CratonVM-sbfull` commit `a245002c`) — verified byte-identical to HotSpot.
**Recommendation:** **FIX** — small, well-scoped VM type-check bug with a one-line repro.

## Fix (landed)

`array_is_assignable_to` (shared by `instanceof`/`checkcast`/`aastore` in both
interpreter and JIT) had a blanket lenient fallback `src_comp == "java/lang/Object"
=> true`, added so native `Object[]`-typed enum/reflection arrays survive a
`checkcast` to `T[]`. Correct for `checkcast`/`aastore`, wrong for `instanceof`.
Threaded a `lenient` flag: `instanceof` uses a strict variant
(`array_is_instance_of`), `checkcast`/`aastore` stay lenient. Same flag through
the JIT `jit_typecheck_resolve` (`jit_instanceof` strict, `jit_checkcast`
lenient). `ArrInstProbe` now byte-identical; enum-array casts still work (verified
no regression on ToStrProbe/WildProbe/RepAnnProbe/CollCopyProbe).

**Binary:** `cvsbfull.exe` (dev `df11ac00`) vs HotSpot `jdk-25`.

## Affected probe

`ArrInstProbe`.

## Symptom

```
                       CratonVM   HotSpot
Object[] instanceof I[]   true       false
```

`ArrInstProbe` allocates a plain `Object[]` and tests `arr instanceof I[]`
(array of an interface type `I`). The element type of the array is `Object`,
which does **not** implement `I`, so per JLS array-store/checkcast rules the
result must be **`false`**. CratonVM answers **`true`**.

## Root cause (hypothesis)

CratonVM's `instanceof`/`checkcast` for **array-of-reference** types is not
checking element-type assignability correctly: it appears to treat
`Object[] <: I[]` as true (covariant the wrong direction). `T[] instanceof S[]`
must hold iff `T <: S` (element assignability), not `S <: T`. Likely in the VM's
`checkcast`/`instanceof` handler for array class IDs — the element-class
assignability test is inverted or skipped when the target element is an
interface.

This is a soundness hole: code relying on `instanceof I[]` (common in Spring's
`ObjectUtils`/array conversion paths) will take the wrong branch and can later
hit a real `ArrayStoreException` mismatch.

## Repro

```java
interface I {}
Object[] a = new Object[1];
System.out.println(a instanceof I[]);   // CratonVM: true ; HotSpot: false
```

```bash
cd C:/craton/CratonVM/apps/spring-boot/buildSrc
CP="runner;$(cat test-classpath.txt)"
"C:/craton/CratonVM-sbfull/target/release/cvsbfull.exe" --java-home "C:/Program Files/Java/jdk-25" -cp "$CP" ArrInstProbe
"C:/Program Files/Java/jdk-25/bin/java.exe" -cp "$CP" ArrInstProbe
```

## Impact

Wrong control flow anywhere `x instanceof SomeInterface[]` is used; latent
`ArrayStoreException` divergence. Self-contained fix in the array
`instanceof`/`checkcast` path.
