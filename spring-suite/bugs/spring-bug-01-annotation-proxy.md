# spring-bug-01: annotation attribute accessors broken (CratonVM `Proxy$Instance` synthesis)

| | |
|---|---|
| **Category** | VM-CORRECTNESS (annotation reflection) — high impact, foundational |
| **Module** | spring-core |
| **CratonVM** | FAIL (and `NoSuchMethodError`/`AbstractMethodError` on annotation accessors) |
| **HotSpot JDK 25** | OK |
| **CratonVM HEAD** | c5644da4 (dev) |
| **Status** | OPEN |
| **Suggested owner** | **me** (foundational, blocks large swaths of Spring) |

## Symptom
CratonVM models annotations / dynamic proxies with a synthetic `java/lang/reflect/Proxy$Instance`
class. Several annotation-attribute operations dispatch incorrectly on it:

- `WARN cratonvm_vm::vm::vm_exec: NoSuchMethodError method="java/lang/reflect/Proxy$Instance.value()Ljava/lang/String;"`
- `AbstractMethodError: method …$MyRepeatable.value()Ljava/lang/String; has no Code attribute`
  (repeatable-annotation container accessor has no body)
- `IllegalStateException: Attribute 'characters' … should be compatible with char[] but a java.lang.Integer[] value was returned`
  (annotation array default of primitive type comes back boxed/wrong-typed)
- `ClassCastException: java/lang/annotation/Annotation cannot be cast to [[Ljava/lang/annotation/Annotation;`
  (nested annotation-array attribute returns wrong shape)
- `AnnotationConfigurationException: … attribute 'path' and its alias 'value' are declared with
  values of [{}] and [{}]` — `@AliasFor` mirror check sees **both** sides as empty `{}`, i.e.
  annotation **default values are not being materialised**.

## Affected test classes (9 confirmed CV-unique, HotSpot OK)
```
core.annotation.AnnotationUtilsTests            (72 found, 8 fail)
core.annotation.AnnotatedMethodTests
core.annotation.AnnotationFilterTests
core.annotation.AnnotationsScannerTests
core.annotation.AnnotationTypeMappingsTests
core.annotation.AnnotationIntrospectionFailureTests
core.annotation.MergedAnnotationClassLoaderTests
core.annotation.MergedAnnotationsRepeatableAnnotationTests
core.annotation.MergedAnnotationsComposedOnSingleAnnotatedElementTests
```
(Several `aot.hint.*` reflection-hint tests likely share this cause — see spring-bug-07.)

## Reproduce
```bash
VM=C:/craton/CratonVM/target/release/cratonvm.exe
JDK='C:\Program Files\Eclipse Adoptium\jdk-25.0.2.10-hotspot'
H=C:/craton/CratonVM-spring/spring-suite
CP="$H;$(tr -d '\r' < C:/craton/cratonvm/apps/spring-framework/spring-core/build/cratonvm-testcp.txt)"
KRUN_STACK=1 "$VM" --java-home "$JDK" -cp "$CP" KRun org.springframework.core.annotation.AnnotationUtilsTests
# HotSpot passes:
"$JDK\bin\java.exe" -cp "$CP" KRun org.springframework.core.annotation.AnnotationUtilsTests
```

## Suspected root cause
CratonVM's synthetic annotation/proxy implementation (`Proxy$Instance`) does not correctly
synthesize per-attribute accessor methods:
1. accessor methods for some annotation attributes are missing a body ("no Code attribute") /
   not registered (`NoSuchMethodError value()`),
2. **default values** are not applied (returns empty `{}` / null instead of the declared default),
3. primitive-array attribute types are returned boxed (`Integer[]` instead of `char[]`),
4. nested-annotation-array attributes return the wrong array rank.

Look at where CratonVM builds the annotation proxy class & its method table (search the
native-builtins / vm proxy code for `Proxy$Instance`, `AnnotationInvocationHandler`,
annotation default-value extraction from the `AnnotationDefault` attribute).

## VERIFIED RESULT (on dev `bc1fa64f`): `AnnotationUtilsTests` 8 → 6 fails
The `$Proxy0` + char[] fixes (commit `168ca1b6`) cleared 2 of the 8 `AnnotationUtilsTests` failures
(now found=72 succ=66 fail=6; was succ=64 fail=8). Remaining 6 are the `@AliasFor`-mirror /
`MergedAnnotation`-synthesis sub-bugs (#2/#3 below) — each needs ~1h in-context tracing (repros +
trace points identified). Real partial progress; foundational `$Proxy0` defect resolved.

## ★ FOUNDATIONAL fix found (sub-bug #0) — first-proxy `$Proxy0` generation fails — FIXED on dev `168ca1b6`
Deep investigation found the dominant, foundational cause: **the FIRST dynamic proxy created in a
process always falls back to a bare `java/lang/reflect/Proxy$Instance`** (no generated member
bodies), so a synthesized annotation's accessor resolves to the abstract interface method →
`AbstractMethodError: <Ann>.value() has no Code` / `NoSuchMethodError Proxy$Instance.value()`.
Root cause: `define_or_get_proxy_class` (`native-builtins/src/lib.rs:35011`) pre-loaded the synthetic
super with `ensure_class_initialized("java/lang/reflect/Proxy$Instance")`, which **fails with
ClassNotFound in real-JDK mode** (`java/*` name + real boot classes present), leaving the super
unresolvable so `$Proxy0`'s `define_class_full` fails → bare fallback. Every proxy *after* the first
works (the fallback allocation registers the stub), so only `$Proxy0` is broken — exactly matching
the single `$Proxy0 … ClassNotFound Proxy$Instance` line in every affected Spring annotation test.
**Fix (STAGED, 1 line):** use the never-fails `ensure_synthetic_class("…Proxy$Instance", 3)`. LOW
risk (same stub already backs `$Proxy1+`). This should clear the repeatable-annotation
`AbstractMethodError` and several other annotation failures. Proven in isolation (3 proxies: only the
first was bare before; all real after). Building/verifying in the batched build.

Remaining sub-bugs #2 (`@AliasFor` empty `[{}]` — the mirror `ValueExtractor` receives the default
instead of the explicit value; standalone repro + trace points identified) and #3 (param-annotation
`Annotation[][]` rank ClassCast) still need ~1h in-context tracing each — re-measure after #0 lands.

## Sub-bug breakdown (investigated this session)
`AnnotationUtilsTests` = 72 found, 8 fail, across 3 distinct sub-causes:

1. **`char[]`/primitive-array attribute coercion → FIX WRITTEN (source applied; build batched).** `synthesizeAnnotationFromDefaultsWithoutAttributeAliases` failed: *"Attribute 'characters' should be compatible with char[] but a java.lang.Integer[] value was returned."* Root cause: `annotation_element_to_java_typed` (`native-builtins/src/lang_class.rs` Array arm) hardcoded the array **component class** to `Integer` for `AEV::Int` elements (which encode Z/B/C/S/I), even though the element *values* were boxed correctly. So `char[]` became an `Integer[]`-typed array. Fixed: derive the component wrapper from the array descriptor (`[C`→Character[], `[Z`→Boolean[], …). Building/verifying.

2. **Repeatable-annotation accessor (`MyRepeatable.value() has no Code`) — NOT a plain-reflection bug.** A minimal repro (`@Repeatable` + `getAnnotationsByType` + `value()`) **passes** under CratonVM (proxy stamped as the annotation interface, accessor dispatched fine via the AnnotationProxy rescue). The failure is specific to Spring's **`MergedAnnotation` synthesis** path (`SynthesizedMergedAnnotationInvocationHandler`), where the synthesized instance's accessor isn't dispatched. Needs in-context tracing — not isolable with a minimal repro.

3. **`@AliasFor` mirror values** (`getAnnotationAttributesWithAttributeAliasesWithDifferentValues`) — also Spring `AnnotationTypeMapping.MirrorSets` synthesis machinery.

## Assessment
The **high-leverage** annotation failures (#2, #3 + the `Annotation→[[Annotation` ClassCast in
`AnnotatedMethodTests`) live in Spring's `MergedAnnotation`/`@AliasFor` **synthesis** layer, deeply
entangled with CratonVM annotation reflection — hard to isolate (minimal repros pass) and needs
tracing inside the full Spring test context. The cleanly-fixable piece (#1 primitive-array
coercion) is fixed. The rest is handoff-grade / a dedicated deep session.

## Notes
Foundational: annotation reflection underpins Spring's entire `core.annotation` +
`MergedAnnotation` machinery, so this cascades across spring-context, spring-test, spring-web, etc.
Likely the same subsystem behind the [[spring-bug-06]] hang and the [[spring-bug-11]] Groovy
`@Generated`-on-Object error. Related: [[spring-bug-08]].
