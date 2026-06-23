# SBR-07 — `getSimpleName()` on a nested class returns `Outer$Inner` instead of `Inner`

**Status:** 🟠 Open — root-caused (deferred; needs the kotlin-reflect repro).
**Recommendation:** FIX — but **not** the trivial one first assumed (see below).

## Root cause (investigated 2026-06-22)

`getSimpleName()` is **correct for ordinary nested classes** — verified
`SNProbe$Inner`→`Inner`, `Foo$Member`→`Member`, `java.util.Map$Entry`→`Entry`
all match HotSpot. The CratonVM `simple_class_name` helper already splits on `$`.

The KProtoProbe divergence comes from CratonVM running the **real JDK
`Class.getSimpleName()` bytecode** (the `$`-splitting native is not active in the
real-JDK path). JDK's `getSimpleName0()` calls `getSimpleBinaryName()` →
`isTopLevelClass()` (`getDeclaringClass0() == null`). For the Kotlin
`...metadata.ProtoBuf$StringTable` classes, CratonVM's `getDeclaringClass0()` /
InnerClasses-attribute resolution returns **null**, so JDK treats the class as
**top-level** and strips only the package (after the last `.`), yielding
`ProtoBuf$StringTable` instead of `StringTable`.

So the real defect is **CratonVM's `getDeclaringClass0()` / InnerClasses
enclosing-class resolution for these specific (protobuf-generated, deeply nested)
classes**, not `getSimpleName` itself. It does not reproduce with javac-compiled
nested classes, so a repro needs `kotlin-reflect` on the classpath
(`buildSrc/runner` + `KProtoProbe`). Fixing `getDeclaringClass0` to populate the
enclosing class from the InnerClasses attribute for these classes would fix it.

**Binary:** `cvsbfull.exe` (dev `df11ac00`) vs HotSpot `jdk-25`.

## Affected probe

`KProtoProbe` (loads Kotlin-metadata nested classes `ProtoBuf$StringTable`,
`ProtoBuf$QualifiedNameTable`).

## Symptom

```java
strings.getClass().getSimpleName()    // strings is a ProtoBuf$StringTable instance
```

```
CratonVM: class_ count=60 strings=ProtoBuf$StringTable qnames=ProtoBuf$QualifiedNameTable
HotSpot:  class_ count=60 strings=StringTable           qnames=QualifiedNameTable
```

`getSimpleName()` must return only the **innermost** identifier (`StringTable`).
CratonVM returns the full `Enclosing$Nested` binary leaf, i.e. it does not strip
the enclosing-class prefix for nested types.

## Root cause (hypothesis)

CratonVM's `Class.getSimpleName()` derives the name from the binary name by
taking the substring after the last `.`, but does **not** additionally strip
everything up to and including the last `$` for nested classes (and does not use
the `InnerClasses`/`EnclosingMethod` attribute). HotSpot strips the binary name
back to the simple identifier. (Anonymous/local class handling should be checked
in the same fix — they also depend on this logic.)

## Repro

```java
class Outer { static class Inner {} }
System.out.println(Outer.Inner.class.getSimpleName());  // CratonVM: Outer$Inner ; HotSpot: Inner
```

```bash
cd C:/craton/CratonVM/apps/spring-boot/buildSrc
CP="runner;$(cat test-classpath.txt)"
"C:/craton/CratonVM-sbfull/target/release/cvsbfull.exe" --java-home "C:/Program Files/Java/jdk-25" -cp "$CP" KProtoProbe
"C:/Program Files/Java/jdk-25/bin/java.exe" -cp "$CP" KProtoProbe
```

## Impact

Any logging, error messages, or logic keyed on `getSimpleName()` of a nested
class diverges (common in framework diagnostics and `toString`). Small fix in the
`getSimpleName` implementation.
