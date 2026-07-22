# SBR-12 — `List.of`/`copyOf`/`unmodifiableList` leak `cratonvm.internal.UnmodifiableList`

**Status:** 🟠 Open — root-caused (deferred; **not** a safe one-liner — see below).
**Recommendation:** FIX via real `ImmutableCollections`, or a guarded name alias.

## Root cause + why it is not a quick rename (investigated 2026-06-22)

`cratonvm/internal/UnmodifiableList` (const `UNMOD_LIST_CLASS`,
`native-collections/src/lib.rs:27272`) is the single synthetic backing class for
**both** `List.of`/`copyOf` (`freeze_result(ctx, UNMOD_LIST_CLASS, …)`) **and**
`Collections.unmodifiableList` (`alloc_unmod_wrapper(ctx, UNMOD_LIST_CLASS, …)`).
The name string is also used as a **dispatch identity** in many natives
(`name == UNMOD_LIST_CLASS` guards for equals/hashCode/iterator/see-through).

Two reasons a simple rename is unsafe:
1. The two factories map to **different** JDK classes
   (`java.util.ImmutableCollections$List12/ListN` vs
   `java.util.Collections$UnmodifiableRandomAccessList`) — one rename can only
   satisfy one of them.
2. Renaming to a **real** JDK name risks **colliding** with the real rt.jar class
   of that name (CratonVM loads real `java.util.ImmutableCollections`); the
   `cratonvm/internal/*` name was chosen precisely to avoid that collision.

Correct fixes (each non-trivial):
- **Preferred:** make `List.of`/`copyOf` return real
  `java.util.ImmutableCollections$List{1,2,N}` instances and
  `unmodifiableList` return a real `Collections$UnmodifiableRandomAccessList`
  (collections rework).
- **Cheaper, still safe:** register two distinct synthetic classes under the JDK
  names as **aliases** of the same native method set, and route each factory to
  the matching one — but verify no collision with any real loaded class.

Functionality is correct today; only `getClass().getName()`/`getSimpleName()`
leak the internal name. Medium effort, not a one-liner.

**Binary:** `cvsbfull.exe` (dev `df11ac00`) vs HotSpot `jdk-25`.

## Affected probes

`LOfHash2`, `LOfHashProbe`, `KListProbe`.

## Symptom

```java
List.of(a, b).getClass().getName()
List.copyOf(x).getClass().getName()
```

```
CratonVM: cratonvm.internal.UnmodifiableList
HotSpot:  java.util.ImmutableCollections$List12   (and $ListN / $List1 by size)
```

`KListProbe` shows the same for the `Collections.unmodifiableList` /
`UnmodifiableRandomAccessList` family (CratonVM omits/replaces the expected
`java.util.Collections$UnmodifiableRandomAccessList`).

> Note: the **hashCode value** differences also seen in `LOfHash2`/`LOfHashProbe`
> are **not** bugs — those lists contain a `Class` object whose `Class.hashCode()`
> is identity-based and legitimately VM-specific. The real, deterministic bug is
> the leaked `getClass().getName()`.

## Root cause (hypothesis)

CratonVM implements the immutable/unmodifiable `List` factories with an internal
Rust-backed class registered under the name **`cratonvm.internal.UnmodifiableList`**,
and exposes that name through `Class.getName()`/`getSimpleName()`. HotSpot's are
`java.util.ImmutableCollections$List{1,2,N}` (for `List.of`/`copyOf`) and
`java.util.Collections$UnmodifiableRandomAccessList` (for `unmodifiableList`).
The internal class should either be named to match the JDK class or, better, the
factories should return real `java.util.ImmutableCollections$*` instances.

## Repro

```bash
cd C:/craton/CratonVM/apps/spring-boot/buildSrc
CP="runner;$(cat test-classpath.txt)"
"C:/craton/CratonVM-sbfull/target/release/cvsbfull.exe" --java-home "C:/Program Files/Java/jdk-25" -cp "$CP" LOfHash2
"C:/Program Files/Java/jdk-25/bin/java.exe" -cp "$CP" LOfHash2
# CratonVM: "a1 class = cratonvm.internal.UnmodifiableList"
# HotSpot:  "a1 class = java.util.ImmutableCollections$List12"
```

## Impact

Any code that switches on the collection's class name, serializes it, or asserts
the JDK immutable-collection type sees a foreign `cratonvm.internal.*` name.
This is an abstraction leak that also masks itself in diff tooling (a normalizer
that drops `cratonvm` lines will hide it — as it initially did here). Well-scoped
fix in the immutable-list factory / class registration.
