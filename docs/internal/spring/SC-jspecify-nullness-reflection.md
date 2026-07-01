# Fixed 2026-07-01: JSpecify type-use & package-level nullness annotations are visible to reflection

## Resolution 2026-07-01
Current `dev` has both gaps from this report implemented:
- `Method.getAnnotatedReturnType()`, `Parameter.getAnnotatedType()`, `Executable.getAnnotatedParameterTypes()`, and `Field.getAnnotatedType()` are registered natively and surface `RuntimeVisibleTypeAnnotations` through `AnnotatedType`.
- `Class.getPackage()` now wires a package `Module` and resolves `<pkg>/package-info`, so package-level declaration annotations such as JSpecify `@NullMarked` / `@NullUnmarked` are visible through `Package`.

This branch adds native-level regression coverage for the TYPE_USE half: method return, indexed method parameter, and field annotations now round-trip through the `AnnotatedType` annotation APIs. The original report below is retained as historical root-cause documentation.

## Symptom
26 `jspecify*` methods in `org.springframework.core.NullnessTests` plus `org.springframework.core.MethodParameterTests.jspecifyNullableParameter` fail with a bare `AssertionError` (no message вЂ” the assertion compares the computed `Nullness` enum against the expected one). In every case CratonVM computes `Nullness.UNSPECIFIED` where the test expects `NULLABLE` or `NON_NULL`.

## Affected tests
All 26 listed in `affected_tests`. They split cleanly along the two root causes below. Tests that derive nullness from a CLASS-level or METHOD-level `@NullMarked`/`@NullUnmarked` declaration annotation (e.g. `jspecifyMethodMarkedUnspecifiedReturnType`, `jspecifyClassMarkedNonNullReturnType`, `jspecifyClassMarkedMethodUnmarkedUnspecifiedReturnType`) and the `customNullable*` / `void*` / `primitiveField` tests are NOT affected вЂ” those paths work вЂ” which precisely bounds the failing set.

## Root cause вЂ” TWO distinct gaps

Spring's `org.springframework.core.Nullness` (spring-core `Nullness.java`) drives all of these. The relevant code is `jSpecifyNullness(...)` which reads:
- type-use annotations via `annotatedType.isAnnotationPresent(Nullable.class/NonNull.class)` where `annotatedType` is `method.getAnnotatedReturnType()`, `parameter.getAnnotatedType()`, or `field.getAnnotatedType()`;
- declaration annotations via `declaringClass.getPackage().isAnnotationPresent(NullMarked.class)`, `declaringClass.isAnnotationPresent(...)`, and `annotatedElement.isAnnotationPresent(...)`.

JSpecify's `@Nullable` and `@NonNull` are `@Target(TYPE_USE)` вЂ” they live in the `RuntimeVisibleTypeAnnotations` attribute, NOT the ordinary `RuntimeVisibleAnnotations`. `@NullMarked`/`@NullUnmarked` are ordinary declaration annotations.

### Bug #1 (dominant): `getTypeAnnotationBytes0()` always returns null
`native-builtins/src/lib.rs:6889-6900` registers both:
- `java/lang/reflect/Executable.getTypeAnnotationBytes0()[B` в†’ `|_,_| Ok(Some(Value::Object(None)))`
- `java/lang/reflect/Field.getTypeAnnotationBytes0()[B` в†’ `|_,_| Ok(Some(Value::Object(None)))`

The JDK implements `Method.getAnnotatedReturnType()`, `Executable.getAnnotatedParameterTypes()` (which backs `Parameter.getAnnotatedType()`), and `Field.getAnnotatedType()` by feeding `getTypeAnnotationBytes0()` to `sun.reflect.annotation.TypeAnnotationParser`. With null bytes the parser yields `AnnotatedType`s carrying zero annotations, so `isAnnotationPresent(Nullable/NonNull)` is always `false`. This drops EVERY JSpecify `@Nullable`/`@NonNull` on return types, parameters, and fields. The module doc comment at `native-builtins/src/lang_reflect.rs:26` even states this is a deliberate "return null (no type annotations tracked yet)" no-op.

There is no native override for `Parameter.getAnnotatedType()` or `Executable.getAnnotatedParameterTypes()`, so they run real JDK bytecode and inherit the null bytes.

### Bug #2: synthetic `Package` carries no `package-info` annotations
`native-builtins/src/lang_class.rs:10331` `native_class_get_package` builds a synthetic `java/lang/Package` populated only with name / module / manifest fields. No native overrides `Package.getDeclaredAnnotations()` / `getAnnotation()` / `isAnnotationPresent()`, so they fall through to the real JDK `Package.packageInfo()` path, which (per the in-code comment at `lang_class.rs:10398-10405`) resolves to the empty sentinel `[]` вЂ” i.e. package-level `@NullMarked`/`@NullUnmarked` declared in `package-info.java` are invisible. This breaks the tests whose only nullness source is the package annotation: `jspecifyPackageMarkedUnspecifiedReturnType`, `jspecifyPackageMarkedNonNullReturnType`, `jspecifyPackageMarkedUnspecifiedParameter`, `jspecifyPackageMarkedNonNullParameter` (and the `*Nullable*`/`*NonNull*` package tests fail on bug #1 too).

Note: class- and method-level declaration annotations DO work вЂ” `native_class_is_annotation_present` (`lang_class.rs:8601`) and `native_method_is_annotation_present` (`lang_class.rs:9090`) read `ctx.class_annotations` / `ctx.method_annotations`, both registered in `lib.rs`. That asymmetry is exactly what the passing-vs-failing split shows.

## The data already exists (fix is tractable)
The reader fully decodes per-method/per-field type annotations: `reader/src/attribute.rs:1377` parses `RuntimeVisibleTypeAnnotations` into `Vec<TypeAnnotation>` (with target_type/target_info raw bytes). The classloading layer exposes them per-member: `classloading/src/annotations.rs:148` `AnnotationsView::type_annotations()`, reachable via `method_annotations(method)` (`annotations.rs:175`) and `field_annotations(field)` (`annotations.rs:181`). What is missing is (a) a `NativeContext` accessor surfacing method/field/parameter type annotations (today only the class-level `raw_type_annotations` exists, `native-api/src/registry.rs:2250`), and (b) wiring the reflection natives to it.

## Suggested fix shape
Two independent pieces:
1. Bug #1: add NativeContext accessors for per-method-return, per-parameter, and per-field type annotations (built from `AnnotationsView::type_annotations()`, filtering by `target_type`: 0x14 method return, 0x16 formal-parameter w/ index, 0x13 field), then EITHER (a) override `Method.getAnnotatedReturnType()`, `Executable.getAnnotatedParameterTypes()`, and `Field.getAnnotatedType()` natively to construct `AnnotatedType` objects directly (mirroring the existing `native_class_get_annotated_superclass` synthetic-AnnotatedType builder in `lang_class.rs`), OR (b) reconstruct correct `getTypeAnnotationBytes0()` bytes вЂ” option (a) is safer since it avoids re-implementing the constant-pool-relative byte format the JDK parser expects.
2. Bug #2: register native `Package.getDeclaredAnnotations()` / `getAnnotation(Class)` / `isAnnotationPresent(Class)` that resolve `<pkg>.package-info` and return its class-level annotations (reuse `ctx.class_annotations(package_info_cid)` + `create_annotation_proxy`), instead of letting the synthetic Package degrade to `[]`.

## Reproduction sketch
Java (needs a TYPE_USE `@Nullable` and a `@NullMarked` package-info on the same package):
```java
import java.lang.reflect.*;
import org.jspecify.annotations.*;
public class Repro {
  public @Nullable String r() { return null; }
  public void p(@Nullable String x) {}
  public static void main(String[] a) throws Exception {
    Method r = Repro.class.getMethod("r");
    System.out.println("ret @Nullable = " + r.getAnnotatedReturnType().isAnnotationPresent(Nullable.class)); // JDK: true; CratonVM: false
    Method p = Repro.class.getMethod("p", String.class);
    System.out.println("param @Nullable = " + p.getParameters()[0].getAnnotatedType().isAnnotationPresent(Nullable.class)); // JDK: true; CratonVM: false
    System.out.println("pkg @NullMarked = " + Repro.class.getPackage().isAnnotationPresent(NullMarked.class)); // with @NullMarked package-info: JDK true; CratonVM false
  }
}
```
Command (do NOT run now вЂ” a full suite is in progress on this box):
```
cratonvm --java-home <jdk25> -cp .:jspecify-1.0.0.jar Repro
```
Expected: three `true`. Observed on CratonVM: three `false`. The simplest in-tree repro is to run `NullnessTests.jspecifyNullableReturnType` (type-use) and `jspecifyPackageMarkedNonNullReturnType` (package) directly.

## Severity / Confidence
Severity medium: reflection-completeness gap in a niche-but-public API (`AnnotatedType` type-use annotations + `Package` annotations). It does not crash and does not affect core execution, but any library relying on type-use or package annotations via reflection (Spring 7's Nullness API, JSpecify/Checker-style tooling, JAXB `@XmlSchema` package annotations) silently gets wrong results. Confidence high: the null-returning natives and the empty synthetic Package are unambiguous in source, the Spring-side consumer code is read in full, and the failing/passing split matches the two gaps exactly.

## Open questions
- For bug #1, is approach (a) native `AnnotatedType` construction preferred over (b) synthesizing `getTypeAnnotationBytes0()` bytes? (a) avoids matching the JDK's constant-pool-relative byte layout and the per-Executable `ConstantPool` the JDK parser consults.
- Does `MethodParameter.forParameter`/`getParameter` in CratonVM return a `Parameter` whose `getDeclaringExecutable()` round-trips to the right `Method`? (The `*WithMethodParameter` and `MethodParameterTests` variants depend on it; appears fine since the same natives back both, but worth a smoke check after the fix.)
- Does `<pkg>.package-info` reliably load on demand when `Class.getPackage()` is the first reference to that package? Bug #2's fix must trigger that load (or read the already-known package-info ClassId) rather than relying on prior resolution.
