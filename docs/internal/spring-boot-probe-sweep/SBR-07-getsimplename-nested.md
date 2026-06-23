# SBR-07 — `getSimpleName()` on a nested class returns `Outer$Inner` instead of `Inner`

**Status:** ✅ FIXED (2026-06-23, dev) — root cause was `getDeclaringClass0()`
not loading the unloaded enclosing class, exactly as predicted below.
**Recommendation:** ~~FIX~~ DONE.

## Fix (2026-06-23)

The defect was confirmed to be in **`getDeclaringClass0()`**, not `getSimpleName`.
`Vm::declaring_class` resolves the enclosing class name through
`find_class_by_name`, which only sees **already-loaded** classes. When a nested
class is resolved by name (`Class.forName("Pkg.Outer$Inner")`) without ever
referencing `Outer`, the outer class is never loaded, so `declaring_class`
returns `None`, `getDeclaringClass0()` returns null, and the real-JDK
`getSimpleName()`/`getCanonicalName()` bytecode treats the class as top-level
(stripping only the package → `Outer$Inner` / `Outer.Outer$Inner`).

Fix in `native-builtins/src/lang_class.rs` (`native_class_get_declaring_class`):
when the fast `ctx.declaring_class` path returns `None`, walk this class's own
`InnerClasses` attribute for the entry naming itself and **load** the recorded
outer class (the same resolve-then-load pattern `getDeclaredClasses0` already
uses), then return its mirror. Anonymous/local classes (empty
`outer_class`/`inner_name`) have no such entry and correctly stay null. HotSpot
loads the enclosing class at this site too. `getCanonicalName` (which recurses
through `getEnclosingClass()` → `getDeclaringClass0()`) is fixed by the same
change.

Verified: `Class.forName("Outer$Inner").getSimpleName()` → `Inner`,
`getCanonicalName()` → `Outer.Inner`, `Inner[]` arrays, anonymous (`""`/null),
local, and top-level classes all byte-identical to HotSpot jdk-25. KProtoProbe:
`strings=StringTable qnames=QualifiedNameTable` == HotSpot. Regression coverage
added in `regression-suite/src/RReflect.java` (sibling `RReflectOuter` resolved
only via `forName`, so the bug-triggering unloaded-outer shape is exercised);
proven to fail on the pre-fix binary and pass after.

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
