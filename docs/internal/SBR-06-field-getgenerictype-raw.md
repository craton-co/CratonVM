# SBR-06 — `Field.getGenericType()` erases generics → returns raw `Class`

**Status:** ✅ **FIXED** (worktree `CratonVM-sbfull` commit `bce39db7`) — verified byte-identical to HotSpot.
**Recommendation:** **FIX** — crisp repro; affects Spring type resolution.

## Correction + fix (landed)

The probe title says "Field.getGenericType" but the divergent line is actually
the **constructor canonical-parameter** print (`Parameter.getParameterizedType()`).
`Field.getGenericType()` and `Constructor.getGenericParameterTypes()` were already
correct; only `Parameter.getParameterizedType()` on a **Constructor** returned the
raw type (`List` not `List<Foo>`). Method parameters were fine.

Root cause: `Parameter.getParameterizedType()` runs JDK bytecode
(`executable.getAllGenericParameterTypes()`), which checks
`hasGenericInformation()` (`getGenericSignature() != null`) **first** and
short-circuits to the erased `getParameterTypes()` when the `signature` field is
null. `create_method_object` populated `signature` from the JVMS §4.7.9 Signature
attribute; `create_constructor_object` did **not**. Added the same population to
the constructor mirror. GenProbe + WildProbe/GenTypeProbe/GenTypeProbe2/DVUProbe/
RepAnnProbe all byte-identical to HotSpot afterward; Method path unaffected.

**Binary:** `cvsbfull.exe` (dev `df11ac00`) vs HotSpot `jdk-25`.

## Affected probe

`GenProbe`.

## Symptom

For a record component `List<TestSlice> slices`, the probe calls
`Field.getGenericType()`:

```
CratonVM: slices field genericType=interface java.util.List   (Class)
HotSpot:  slices field genericType=java.util.List<GenProbe$TestSlice>  (ParameterizedTypeImpl)
                                                                  args=[class GenProbe$TestSlice]
```

- **HotSpot:** returns a `ParameterizedType` carrying the actual type argument
  `GenProbe$TestSlice`.
- **CratonVM:** returns the **raw `Class` `java.util.List`** — the generic
  signature is dropped entirely; `getGenericType() instanceof ParameterizedType`
  is false.

## Root cause (hypothesis)

CratonVM does not parse the field's `Signature` attribute (the generic type
signature) — it falls back to the erased descriptor type. The `Signature`
attribute parser / `ParameterizedTypeImpl` synthesis is missing or not wired for
`Field.getGenericType()` (and likely `Method.getGenericReturnType()` /
`getGenericParameterTypes()` — worth checking together).

## Repro

```bash
cd C:/craton/CratonVM/apps/spring-boot/buildSrc
CP="runner;$(cat test-classpath.txt)"
"C:/craton/CratonVM-sbfull/target/release/cvsbfull.exe" --java-home "C:/Program Files/Java/jdk-25" -cp "$CP" GenProbe
"C:/Program Files/Java/jdk-25/bin/java.exe" -cp "$CP" GenProbe
```

## Impact

Spring's `ResolvableType` / `GenericTypeResolver` and Jackson/Binder type
introspection rely on `getGenericType()` returning a `ParameterizedType`.
Erasure to raw `Class` silently degrades generic-aware binding and converter
selection. Well-scoped to the generic-signature reflection path.
