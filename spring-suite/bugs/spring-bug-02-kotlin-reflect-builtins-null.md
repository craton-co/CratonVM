# spring-bug-02: kotlin-reflect `getBuiltInClassByFqName must not return null`

| | |
|---|---|
| **Category** | VM-CORRECTNESS (reflection / metadata) |
| **Module** | spring-core (Kotlin tests) |
| **CratonVM** | FAIL — `IllegalStateException` from kotlin-reflect internals |
| **HotSpot JDK 25** | OK |
| **CratonVM HEAD** | c5644da4 (dev) |
| **Status** | **REPORTED SYMPTOM RESOLVED** by cf58ea48 (SB-15) — see Update 2026-06-15 |
| **Suggested owner** | handoff candidate (Kotlin-reflect deep stack; may be low priority for Spring-Java) |

## Update 2026-06-15 (verified on dev ce325da5, post-SB-15)
The reported `getBuiltInClassByFqName must not return null` ISE is **fixed** by commit
cf58ea48 (SB-15: register `java.lang.Module.getResourceAsStream`), which landed 13 min
after this report was written. Verified with a fresh dev build:
- **0** occurrences of `getBuiltInClassByFqName must not return null` across all 8 classes.
- `BridgeMethodResolverKotlinTests` now **2/2 OK** (was FAIL).

The 8 classes **still fail**, but with a *different, CV-unique* root cause: kotlin-reflect
now loads and runs, but returns **wrong metadata**. Example `MethodParameterKotlinTests`
(CratonVM 6/11, HotSpot **11/11 OK**), all `org.opentest4j.AssertionFailedError`:
- *Suspending function return type* — expected `java.lang.Number` but was `java.lang.Object`
  (Continuation<Number> type arg not recovered from Kotlin metadata).
- *Continuation parameter name for suspending function* — expected `null` but was `"$completion"`
  (synthetic continuation param not filtered).
- *Method return type nullability* — boolean nullability flipped.
- *Inner class constructor*, *Method parameter with default value* — AssertionFailedError.

These are a **new bug class** (kotlin-reflect metadata correctness), NOT the builtins-loading
ISE this doc describes. Recommend tracking as a separate bug (spring-bug-02b / kotlin-reflect
metadata). No `@NotNull`/builtins error remains.

## Update 2026-06-15b — ROOT-CAUSED & PRIMARY FIX LANDED (branch fix/kotlin-reflect-builtins-null)
The metadata-correctness failures had **one** dominant root cause: CratonVM materialized
**primitive annotation arrays as boxed wrapper arrays**. `@kotlin.Metadata.mv()` (metadata
version, declared `int[]`) returned `Integer[]` not `int[]`; kotlin-reflect reads it with
`iaload`, so the boxed array read back as all-zeros → version `(0,0,0)` → kotlin-reflect treated
the class metadata as invalid/legacy and fell back to Java platform types (wrong nullability,
`isSuspend=false`, lost default-value flags, exposed `$completion`). Verified: raw `@Metadata`
payload was byte-identical to HotSpot except `mv`.

**Fix** (`native-builtins/src/lang_class.rs`, `annotation_element_to_java_typed`): build real
primitive arrays for `[I`/`[Z`/`[J`/`[B`/`[C`/`[S`/`[F`/`[D` return descriptors instead of boxed
wrappers. **Result: spring-core Kotlin tests 16 → 4 failures; NullnessKotlinTests,
KotlinReflectionParameterNameDiscovererTests now fully green; MethodParameterKotlinTests 6→10/11.**
Also a net **+1** on the pure-Java `AnnotationUtilsTests` (fixed the `char[]`-vs-`Integer[]`
`synthesizeAnnotationFromDefaultsWithoutAttributeAliases` failure) — **no regressions** (baseline
already FAIL/TIMEOUT on those classes).

**Remaining 4 failures = a SECOND, distinct root cause (spring-bug-02b):** CratonVM's generic-type
reflection **drops the innermost type arguments at nesting depth ≥2**. Fully characterized as a
GENERAL (non-Kotlin) VM bug with a minimal pure-Java repro:

```java
public void direct(Map<String, List<Number>> x) {}            // inner List<Number>
public void sup(Consumer<? super List<Number>> x) {}          // wildcard lower bound
// CratonVM getGenericParameterTypes():
//   direct → Map<String, List<>>          (inner ParameterizedType, args EMPTY)
//   sup    → Consumer<? super List>        (bound reified as RAW Class, not List<Number>)
// HotSpot: List<java.lang.Number> in both cases.
```

Isolation results: (1) CratonVM's own signature parser is CORRECT (added+passed a
`reader/src/signature.rs` unit test for `Continuation<? super Producer<? extends Number>>`, then
reverted it). (2) `vm_exec::method_signature` returns the Signature attribute **verbatim**, and the
reader decodes it as the full pool Utf8 (`attribute.rs` `get_utf8_arc`) — so the **input signature
string is complete and correct**. (3) The reflected types are real-JDK
`sun.reflect.generics.reflectiveObjects.*`, i.e. CratonVM's synthetic `generics.rs` path is NOT
used here. **Conclusion: the defect is in the real-JDK `sun.reflect.generics` SignatureParser/Reifier
*executing under CratonVM*** — a bytecode-execution bug that loses the deepest type-argument level.
Needs VM-side tracing of that reflection path to pin (interpreter/JIT). Affects
GenericTypeResolverKotlinTests + the suspend-generic `MethodParameterKotlinTests` cases, and any
Java code relying on ≥2-deep generic reflection. Diagnostic probes left in `spring-suite/`
(KReflectProbe, GenProbe2, etc.).

## Symptom
Every Spring Kotlin test that drives `kotlin-reflect` throws:
```
java.lang.IllegalStateException: @NotNull method
  kotlin/reflect/jvm/internal/impl/builtins/KotlinBuiltIns.getBuiltInClassByFqName must not return null
  at …JavaToKotlinClassMapper.mapJavaToKotlin(JavaToKotlinClassMapper.kt:41)
  at …JavaTypeResolver.mapKotlinClass / computeTypeConstructor / computeSimpleJavaClassifierType
```
kotlin-reflect's built-in class table lookup returns `null` under CratonVM where it must not,
tripping Kotlin's `@NotNull` intrinsic check.

## Affected test classes (8 confirmed CV-unique, HotSpot OK)
```
core.NullnessKotlinTests
core.MethodParameterKotlinTests
core.GenericTypeResolverKotlinTests
core.BridgeMethodResolverKotlinTests
core.DefaultParameterNameDiscovererKotlinTests
core.KotlinReflectionParameterNameDiscovererTests
core.PropagationContextElementTests
aot.hint.BindingReflectionHintsRegistrarKotlinTests
```

## Reproduce
```bash
CP="$H;$(tr -d '\r' < .../spring-core/build/cratonvm-testcp.txt)"
KRUN_STACK=1 "$VM" --java-home "$JDK" -cp "$CP" KRun org.springframework.core.MethodParameterKotlinTests
"$JDK\bin\java.exe" -cp "$CP" KRun org.springframework.core.MethodParameterKotlinTests   # passes
```

## Suspected root cause
kotlin-reflect builds its built-in class table by reading `.kotlin_builtins` metadata resources
and/or by reflecting over `java.lang.*` mappings. The null return suggests CratonVM either:
- fails to load a classpath **resource** kotlin-reflect reads (`getResourceAsStream` on the
  `kotlin/kotlin.kotlin_builtins` metadata), or
- returns wrong results from a reflection API kotlin-reflect depends on (e.g. `Class.getName`
  for a primitive / array, package introspection).

## Notes
Likely **one** root cause across all 8. Lower priority than [[spring-bug-01]] for pure-Java Spring,
but it blocks all Kotlin-facing Spring features. Good handoff candidate.
