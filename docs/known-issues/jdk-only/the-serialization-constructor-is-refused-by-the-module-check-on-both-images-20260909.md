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
