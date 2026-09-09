# RETIRED — FIXED 2026-09-09. The serialization constructor is no longer refused by the module check

**Retired from**
`docs/known-issues/jdk-only/the-serialization-constructor-is-refused-by-the-module-check-on-both-images-20260909.md`,
written earlier the same day while verifying the JDK 21 serialization fix. The
page below is kept verbatim, including the two things it got wrong, because
both were reasonable readings of the evidence it had and both cost time.

## The cause

`ReflectionFactory.generateConstructor` calls `setAccessible(true)` on the
constructor it has just generated — serialization must construct types nobody
opened. CratonVM's caller walk skipped that frame:
`REFLECTION_INTERNAL_CLASSES` contains both `jdk/internal/reflect/` and
`sun/reflect/`, so **both** `ReflectionFactory` frames were treated as
reflection plumbing, the walk continued out to the application class, and the
accessor was judged to be the unnamed module.

```
SerTrace.show                                                     <- resolved as the caller
sun.reflect.ReflectionFactory.newConstructorForSerialization              skipped
jdk.internal.reflect.ReflectionFactory.newConstructorForSerialization     skipped
jdk.internal.reflect.ReflectionFactory.generateConstructor                skipped
  -> c.setAccessible(true)                                ReflectionFactory.java:437
```

That skip list's own comment asserted the classes left under `sun/reflect/` are
"none of which sit between a caller and a reflection native". `ReflectionFactory`
is precisely that — the same failure shape as the `sun.reflect.misc.MethodUtil`
carve-out sitting one entry above it, one package over.

## Two corrections to the page below

**It named the wrong throw site.** Section "Where to look" sends the reader to
the JPMS deep-reflection arm ("NEW-19") of
`native_constructor_new_instance`, on the premise that `c.newInstance()` throws.
It does not. `probes/SerTrace.java` prints the stack: the throw is inside
`newConstructorForSerialization` itself, at `generateConstructor` line 437. No
constructor is ever returned, so that arm is never reached and the accessible
flag it reads was never the question.

**It named the wrong tell.** The page reads "`java.lang` passes and `java.util`
does not" as evidence of a package-level `opens` test. The discriminator is the
CONSTRUCTOR, not the package. `newConstructorForSerialization` targets the
no-arg constructor of the first non-serializable ancestor:

```
Integer    -> java.lang.Object       modifiers 0x1  public     -> exports arm, never asks about opens
ArrayList  -> java.util.AbstractList modifiers 0x4  protected  -> opens arm
HashMap    -> java.util.AbstractMap  modifiers 0x4  protected  -> opens arm
```

A `java.util` class whose ancestor constructor were public would have passed
too, and that is why the package reading survived: the two example classes
differ in package AND in modifier at the same time.

## The fix

Two EXACT class names added to `REFLECTION_INTERNAL_EXCEPTIONS`:
`sun/reflect/ReflectionFactory` and `jdk/internal/reflect/ReflectionFactory`.
Exact names, not a package prefix — the existing comment is right that a prefix
would be fail-open the moment another accessor-like class lands in either
package.

## Why this is not fail-open, measured

`probes/SerGuard.java` asks for deep access **directly** from an ordinary
classpath class, with no `ReflectionFactory` anywhere on the stack:

```
                                  HotSpot 21   CratonVM 21 --jdk-only
java.util.ArrayList.elementData   DENIED       DENIED
java.lang.String.value            DENIED       DENIED
java.util.HashMap.table           DENIED       DENIED
newConstructorForSerialization    OK           OK
```

Exposing the frame grants exactly what HotSpot grants and nothing more:
`ReflectionFactory` calls `setAccessible` only on a `Constructor` it generated
itself. Three unit tests pin both directions, including controls that the real
accessor plumbing (`NativeMethodAccessorImpl`, `Method`, `Constructor`,
`AccessibleObject`, `Class`) stays skipped — without those, an exception that
matched too much would attribute every `Method.invoke` to plumbing and trust
user code.

## Scope

This was never strict-mode-specific: it failed in `--real-jdk` and `--jdk-only`,
on JDK 21 and JDK 25 alike. Kryo, XStream, Objenesis and several ORMs allocate
through this entry point, so the blast radius was wider than the one probe that
found it.

---

*Original page follows, unedited — including the two claims corrected above.*

# The serialization constructor is refused by CratonVM's module check, on BOTH JDK images

**Status:** open, found 2026-09-09 while verifying the JDK 21 serialization fix.
**Applies to:** JDK 21 AND JDK 25. This is NOT version-dependent.
**Modes:** both `--real-jdk` and `--jdk-only`.
**Severity:** lower than it looks -- see "why this is not the round-trip defect".

## The row

`ReflectionFactory.newConstructorForSerialization(cl).newInstance()` is refused
for any `cl` whose package `java.base` does not `opens`:

```
  B THREW target java.util.ArrayList ->
      java.lang.reflect.InaccessibleObjectException: Unable to make member
      accessible: module java.base does not "opens java.util" to unnamed module
      (use --add-opens to grant access)
  B THREW target java.util.HashMap -> (same)
```

HotSpot allows both, on both images, with no `--add-opens`:

```
                       HotSpot 21   HotSpot 25   CratonVM 21   CratonVM 25
Integer  (java.lang)   OK           OK           OK            OK
ArrayList (java.util)  OK           OK           THREW         THREW
HashMap  (java.util)   OK           OK           THREW         THREW
```

`java.lang` passes and `java.util` does not, which is the tell: the check being
applied is a package-level `opens` test.

## Why HotSpot does not refuse it

The `Constructor` that `newConstructorForSerialization` returns is not an
ordinary reflective constructor. `ReflectionFactory.generateConstructor` builds
it and marks it accessible itself, precisely because serialization has to
construct types nobody opened. Re-running the module check against the *calling*
module therefore asks a question the JDK already answered -- and answers it
differently, because the caller is the unnamed module.

The same mistake is easy to make from the Java side: calling `setAccessible(true)`
on that constructor from a probe reproduces the identical
`InaccessibleObjectException` **on HotSpot**, which is a good way to convince
yourself the VM is right when it is not. Do not add that call to a probe.

## Why this is not the round-trip defect

`ObjectInputStream` does not reach this path -- it holds the constructor
`ObjectStreamClass` built and invokes it internally, so part A of `SerChk.java`
(`ArrayList` and `HashMap` round-trips) passes on both images while part B
throws. The user-visible serialization contract is intact. What is broken is the
`sun.reflect.ReflectionFactory` **public entry point**, which is
`jdk.unsupported` API that real libraries do use directly (this is how
Kryo, XStream, Objenesis and several ORMs allocate).

Because the failure is loud and identical on both images, it is a much safer row
than the one it was found next to: nothing gets a plausible wrong object.

## Reproducing

`SerChk.java` part B; the short form:

```java
Constructor<?> c = ReflectionFactory.getReflectionFactory()
        .newConstructorForSerialization(java.util.ArrayList.class);
System.out.println(c.newInstance().getClass());   // HotSpot: java.util.ArrayList
                                                  // CratonVM: InaccessibleObjectException
```

Do NOT call `c.setAccessible(true)` first -- see above.

## Where to look

`native-builtins/src/lang_class.rs::native_constructor_new_instance`, the
JPMS deep-reflection arm ("NEW-19") below the serialization arms. It reads the
accessible flag out of a CratonVM extra slot; the question to answer first is
whether the flag the JDK set on this constructor survives into that slot, or
whether the VM never sees it because `Constructor.setAccessible` was called by
JDK code through a path that does not write it.

Related: `docs/internal/retired/jdk-21-serialization-round-trip-returns-the-wrong-class-FIXED-20260909.md`
(found while verifying that fix; different mechanism, adjacent code).
