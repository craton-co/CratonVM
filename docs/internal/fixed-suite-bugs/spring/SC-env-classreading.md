# SC-env-classreading — Environment + class-reading metadata cluster

> **TRIAGE 2026-06-22 (against current `dev`, repro `test_classes/EnvClassreadingRepro`).**
> The doc was originally static-analysis-only (disk full). Verified status:
> - **RC-C (`int.class` → wrapper)**: ✅ **already fixed on `dev`** — `int.class == Integer.TYPE`,
>   `int.class.isPrimitive()`, `getName()=="int"`, and `int.class != Integer.class` all hold.
>   The primitive↔wrapper conflation no longer reproduces. (No further action; left documented
>   for history.)
> - **RC-A (`System.getenv()`/`getProperties()` singleton identity)**: ✅ **FIXED on `dev`**
>   (`fea93ba8`, merge `d236eb42`). Both no-arg accessors now return a process-wide cached
>   singleton ObjectRef (GC-rooted + remapped like the singleton class loaders); `getProperties`
>   resyncs its side-table to the live `list_system_properties()` snapshot each call (wholesale
>   replace) so enumeration/`getProperty` stay live and reflect `clearProperty`. Verified vs
>   HotSpot (JDK 25, `test_classes/EnvSingletonRepro` 10/10; holds under GC stress + moving young-gen).
> - **RC-B (`Object.equals` native shadows a bytecode override → `COWAL.indexOf`/`contains`/
>   `remove`)**: 🔴 **OPEN** (reproduces). HIGH severity but **HANDOFF** — a core
>   interpreter/JIT virtual-dispatch + native-shadowing fix (resolve the most-derived `equals`
>   before serving the `java/lang/Object.equals` native); overlaps the tracked
>   "Object.toString/equals/hashCode intrinsic shadowing" work. Not a localized patch.
> - **RC-D (`getResourceAsStream` via user-defined `ClassLoader` subclass)**: 🔴 **OPEN**
>   (reproduces — returns null). HANDOFF (classloader/resource delegation model).

Two distinct root causes (three, if you split classreading), spanning
`org.springframework.core.env` (System property/env access) and
`org.springframework.core.type.classreading` (Spring's ASM-based metadata reader).

## Symptom

| Test class | Failing test | Reported error |
|---|---|---|
| `StandardEnvironmentTests` | `getSystemEnvironment()` | `AssertionError` — content correct but `isSameAs(System.getenv())` fails (identity) |
| `StandardEnvironmentTests` | `getSystemProperties()` | `AssertionError` — content correct but `isSameAs(System.getProperties())` fails (identity) |
| `StandardEnvironmentTests` | `propertySourceOrder()` | `AssertionFailedError: expected: 0 but was: -1` |
| `SimpleAnnotationMetadataTests` | `getAnnotationAttributeIntType()` | `AssertionFailedError: expected: [int] but was: [java.lang.Integer]` |
| `SimpleAnnotationMetadataTests` | `getClassAttributeWhenUnknownClass()` | `FileNotFoundException: class path resource [...$WithClassMissingFromClasspath.class] cannot be opened because it does not exist` |
| `DefaultAnnotationMetadataTests` | `getAnnotationAttributeIntType()` | same as Simple (shared base `AbstractAnnotationMetadataTests`) |
| `DefaultAnnotationMetadataTests` | `getClassAttributeWhenUnknownClass()` | same as Simple |

`getAnnotationAttributeIntType` and `getClassAttributeWhenUnknownClass` are defined once in the
shared base `AbstractAnnotationMetadataTests` and run under both reader factories, so they are
ONE root cause each (not four).

## Affected tests

- env: `StandardEnvironmentTests.getSystemEnvironment`, `.getSystemProperties`, `.propertySourceOrder` (3)
- classreading: `SimpleAnnotationMetadataTests` + `DefaultAnnotationMetadataTests`, methods
  `getAnnotationAttributeIntType` and `getClassAttributeWhenUnknownClass` (2+2 = 4)

Run log: `spring-suite/full-run/spring-core.raw.log` lines 407-417 (env) and 1190-1204 (classreading).

---

## Root cause A (env) — `System.getenv()` / `System.getProperties()` return a fresh object every call (no singleton identity)

Spring's `AbstractEnvironment.getSystemEnvironment()` returns `(Map) System.getenv()` and
`getSystemProperties()` returns `(Map) System.getProperties()`
(`spring-core/.../env/AbstractEnvironment.java:440,446`). The tests assert reference identity:

```java
assertThat(System.getenv()).isSameAs(systemEnvironment);    // getSystemEnvironment()
assertThat(System.getProperties()).isSameAs(systemProperties); // getSystemProperties()
```

CratonVM allocates a brand-new object on every call:

- `System.getProperties()` — `native-builtins/src/lib.rs:2438-2468`: the lambda does
  `crate::alloc_concurrent_synthetic(ctx, "java/util/Properties", 16)` + repopulates the
  side-table on EVERY invocation. No cached singleton.
- `System.getenv()` (no-arg `()Ljava/util/Map;`) — `lang_system::native_system_getenv_all`
  (`native-builtins/src/lang_system.rs:1467-1629`): builds a fresh `java/util/HashMap`
  (real-layout or legacy fallback) from `std::env::vars()` on EVERY invocation. No cache.

Each of the two calls in a test therefore yields a different `ObjectRef`, so `isSameAs`
(reference equality) fails even though the map CONTENTS are correct (the log shows the real
env/props values).

HotSpot returns the same cached `Collections.unmodifiableMap(ProcessEnvironment.theEnvironment)` /
the singleton `System.props`-backed `Properties` on every call, hence `isSameAs` holds.

## Root cause B (env) — `MutablePropertySources.precedenceOf(...)` returns -1 because `indexOf`/`remove(Object)` does not dispatch the overridden `equals`

`propertySourceOrder()` fails at its FIRST assertion:

```java
sources.precedenceOf(PropertySource.named(SYSTEM_PROPERTIES_PROPERTY_SOURCE_NAME))  // expected 0, got -1
```

`precedenceOf` is `this.propertySourceList.indexOf(propertySource)`
(`MutablePropertySources.java:149-150`); `propertySourceList` is a `CopyOnWriteArrayList`
(field decl line 43). `PropertySource.named(name)` returns a `ComparisonPropertySource` whose
`equals` compares only by name (`PropertySource.java:140-143`). `indexOf` works only if the
real COWAL bytecode's element-vs-argument `equals` call dispatches to the `PropertySource.equals`
override.

CratonVM registers a native `java/lang/Object.equals` (`native-builtins/src/lib.rs:3303-3327`)
that does identity-`==` (plus a reflection-stub special case). When the real
`CopyOnWriteArrayList.indexOf` bytecode invokes `equals` on the stored `PropertySource`
elements, the call resolves to this `Object.equals` native (identity) instead of walking to the
`PropertySource.equals(Object)` override — so name-equality never runs, `indexOf` returns -1, and
`precedenceOf` yields -1.

This exact bug is already documented in CratonVM's own shim comments:
`native-builtins/src/spring_startup_bootstrap.rs:152-167` —
"`indexOf` requires the placeholder PropertySource (named-only) and the real
SystemEnvironmentPropertySource entry stored in the COWAL to compare equal — but equality runs
against placeholder/element pairs whose runtime classes have not had `equals` linked to the
override on `PropertySource`. The list IS populated ... but `assertPresentAndGetIndex` still
fails." The shim only patches `MutablePropertySources.replace/addBefore/addAfter` natively
(lib.rs:1002-1017) — it does NOT patch `precedenceOf`/`indexOf`, so the direct-`precedenceOf`
test still reproduces the underlying VM bug.

(Note: `CopyOnWriteArrayList` has natives for `addIfAbsent/contains/bulkRemove/addAll`
— `native-collections/src/lib.rs:31565-31588` — but NOT `indexOf`/`remove(Object)`, so those
run real bytecode and hit the equals-dispatch gap. The "-1" rules out a missing-source theory:
the systemProperties source IS in the list, just invisible to `indexOf`.)

This is a general native-`Object.equals`-shadows-override correctness bug (same family as
memory notes "Optional.equals/hashCode native shadowing" and "spring-bug-08 Object.equals
intrinsic still shadows in some paths"), not env-specific.

---

## Root cause C (classreading) — primitive class literal `int.class` resolves to `java.lang.Integer` (wrapper) instead of `int` (primitive Class)

`getAnnotationAttributeIntType()` reads `@ComplexAttributes(... types = int.class ...)` off
`WithIntType` (`AbstractAnnotationMetadataTests.java:479-483`) and asserts:

```java
assertThat(attributes.get("types").get(0)).isEqualTo(new Class[]{int.class});
// expected: [int]  but was: [java.lang.Integer]
```

Spring's ASM reads the descriptor `I`, derives class name "int", and materialises the Class via
`ClassUtils.forName("int", cl)` → which returns the `int.class` literal, compiled as
`getstatic java/lang/Integer.TYPE`. So the result depends on `Integer.TYPE` resolving to the
`int` primitive mirror.

Two CratonVM sites conflate the primitive mirror with the wrapper:

1. `native-builtins/src/phases_early.rs:1906-1912` (`clinit_integer`, registered at
   `java/lang/Integer.<clinit>`, line 1969) writes the `int` primitive mirror to static field
   **index 0**. The comment says "Synthetic stubs have a TYPE static field at index 0" — but in
   real-JDK mode the real `java.lang.Integer` layout does NOT have `TYPE` at index 0 (it has
   `MIN_VALUE`, `MAX_VALUE`, etc. ahead of it). If this native shadows the real `Integer.<clinit>`,
   the real `TYPE` slot is left unset (or a different static is corrupted), so the
   `getstatic Integer.TYPE` in `int.class` reads the wrong/empty slot.

2. The primitive-class resolver `phases_late.rs:39556-39588` reads `resolve_field_index(wrapper,
   "TYPE")` + `get_static_field`, and on failure **falls back to the wrapper class mirror itself**
   (lines 39586-39588 `get_class_mirror(class_id)` for `java/lang/Integer`). That fallback is
   exactly the observed `[java.lang.Integer]` value.

Reference-type Class attributes work (`getComplexAttributeTypesReturnsAll` with
`types = {TestEnum.class}` passes), which isolates the defect to the primitive `int.class` /
`Integer.TYPE` path. Same primitive↔wrapper conflation family as the memory note "Boxed
primitive annotation arrays broke kotlin-reflect" (lang_class.rs: int[]→Integer[]).

## Root cause D (classreading) — `ClassLoader.getResourceAsStream` on a user-defined ClassLoader subclass returns null for a class-file resource that exists on the classpath

`getClassAttributeWhenUnknownClass()` builds a `FilteringClassLoader extends
OverridingClassLoader` and asks the metadata reader for
`...$WithClassMissingFromClasspath`. Spring resolves
`classpath:org/springframework/core/type/classreading/<Outer>$WithClassMissingFromClasspath.class`
(`AbstractMetadataReaderFactory.java:57-62`, `ClassUtils.convertClassNameToResourcePath`), then
`ClassPathResource.getInputStream()` does
`is = this.classLoader.getResourceAsStream(this.absolutePath)` and throws
"... cannot be opened because it does not exist" when `is == null`
(`ClassPathResource.java:205-212`). The classLoader is the test's `FilteringClassLoader`.

The `.class` file DOES exist on the test classpath:
`spring-core/build/classes/java/test/org/springframework/core/type/classreading/DefaultAnnotationMetadataTests$WithClassMissingFromClasspath.class`
(verified on disk), and that dir is on `java.class.path` (raw log line 414). The 47 passing
tests in the same class read sibling nested classes (e.g. `WithComplexAttributeTypes`) fine —
they go through `source.getClassLoader()` (the default app/system loader). The 2 failing tests
are the ONLY ones that route through a freshly-constructed custom `ClassLoader` subclass.

CratonVM's `ClassLoader.getResourceAsStream` native (`native-builtins/src/classloader.rs:2635-2664`,
registered at lib `classloader.rs:4142-4147`) ignores the receiver `this` and calls
`ctx.find_resource(name)`, which is NOT classloader-aware — it scans the GLOBAL classpath
(`vm/src/vm/vm_exec.rs:5639-5641` → `class_manager.find_resource`,
`classloading/src/class_manager.rs:3487-3493`, bootstrap→extension→application). Since `find_resource`
serves the same global classpath for the default loader (and that works for the passing tests),
the only way this returns null for the custom-loader path is that the native is NOT firing for the
user-defined subclass receiver (the real `ClassLoader.getResourceAsStream` bytecode runs instead,
which calls `this.getResource(name)` → `findResource`/parent chain that CratonVM does not resolve
for a user-defined loader with no real `URLClassPath`). Either way: a class-file resource that the
default loader serves is not served when requested through a user-defined `ClassLoader` subclass.

The expected (HotSpot) behaviour: the bytes ARE found and parsed; the test's real assertion is
that the SUBSEQUENT `getClassArray("types")` throws `TypeNotPresentException` for the filtered
`javax.annotation.meta.When` class — never a FileNotFoundException opening the `.class` file.

---

## Reproduction sketch

### Env A (identity)
```java
public class P {
  public static void main(String[] a) {
    System.out.println(System.getenv() == System.getenv());       // HotSpot: true; CratonVM: false
    System.out.println(System.getProperties() == System.getProperties()); // HotSpot: true; CratonVM: false
  }
}
```
`cratonvm --java-home <jdk25> P`

### Env B (precedenceOf)
```java
import org.springframework.core.env.*;
public class P {
  public static void main(String[] a) {
    var env = new StandardEnvironment();
    var s = env.getPropertySources();
    System.out.println(s.precedenceOf(PropertySource.named(
        StandardEnvironment.SYSTEM_PROPERTIES_PROPERTY_SOURCE_NAME))); // HotSpot: 0; CratonVM: -1
  }
}
```
Minimal (no Spring) repro of the equals-dispatch core:
```java
import java.util.concurrent.CopyOnWriteArrayList;
class K { final String n; K(String n){this.n=n;}
  public boolean equals(Object o){ return o instanceof K k && k.n.equals(n); }
  public int hashCode(){ return n.hashCode(); } }
public class P { public static void main(String[] a){
  var l = new CopyOnWriteArrayList<K>(); l.add(new K("x"));
  System.out.println(l.indexOf(new K("x"))); // HotSpot: 0; CratonVM (expected): -1
}}
```

### Classreading C (int.class)
```java
public class P { public static void main(String[] a){
  System.out.println(int.class);                 // HotSpot: int; CratonVM: class java.lang.Integer (likely)
  System.out.println(int.class == Integer.TYPE); // HotSpot: true
}}
```

### Classreading D (resource via custom loader)
```java
public class P { public static void main(String[] a) {
  ClassLoader cl = new ClassLoader(P.class.getClassLoader()){};
  var is = cl.getResourceAsStream("P.class");    // HotSpot: non-null; CratonVM: null (expected)
  System.out.println(is);
}}
```
`cratonvm --java-home <jdk25> -cp <dir with P.class> P`

## Suspected subsystem

- A: `native-builtins` (System natives: `lang_system.rs`, `lib.rs` getProperties lambda) — missing singleton cache.
- B: VM method dispatch / native shadowing — `java/lang/Object.equals` native (`lib.rs:3303`) shadowing the bytecode override during `CopyOnWriteArrayList.indexOf`/`remove`.
- C: `native-builtins` primitive-class / wrapper `TYPE` resolution (`phases_early.rs` clinit, `phases_late.rs` primitiveClass fallback) + `Integer.TYPE` real-layout field index.
- D: `native-builtins`/`classloading` resource loading — `ClassLoader.getResourceAsStream` not classloader-aware / not firing for user-defined subclasses (`classloader.rs:2635`, `class_manager.rs:3487`).

## Severity

- A (identity): MEDIUM. Narrow API contract (`System.getenv()/getProperties()` singleton identity); breaks any code relying on `==`/`isSameAs` or on mutating the returned `Properties` and seeing it reflected globally (the `getSystemProperties` test also exercises `System.getProperties().put(nonStringKey,...)` round-trip, which the synthetic Properties likely cannot honour either).
- B (equals dispatch): HIGH. A general correctness bug — native `Object.equals` shadows user/library `equals` overrides during JDK collection operations (`indexOf`, `remove(Object)`, `contains` on lists without a native). Wide blast radius beyond Spring.
- C (int.class): MEDIUM-HIGH. Primitive↔wrapper conflation in a fundamental reflection primitive (`int.class`/`Integer.TYPE`); affects any annotation/reflection metadata referencing primitive class literals.
- D (resource via custom loader): MEDIUM. User-defined ClassLoader resource lookup is a common Spring/OSGi/agent pattern; here it blocks ASM metadata reading through any custom loader.

## Confidence

- A: HIGH — the allocating lambdas/functions are unambiguous; assertion is pure identity; log confirms correct content.
- B: HIGH — `expected 0 / got -1`, COWAL has no `indexOf` native, the `Object.equals` native exists, and CratonVM's own shim comments document this exact equals-dispatch gap.
- C: MEDIUM-HIGH — `[int] but was [java.lang.Integer]` matches the wrapper-mirror fallback in `phases_late.rs:39586` and the index-0 `TYPE` write in `phases_early.rs:1909`; could not execute to confirm which of the two fires in real-JDK mode (disk full; static analysis only).
- D: MEDIUM — file confirmed on disk + classpath, native confirmed receiver-blind; could not trace whether the native fires or is bypassed for the user-defined subclass (static only).

## Recommendation

- A — FIX. Cache a process-wide singleton for the no-arg `System.getenv()` Map and the
  `System.getProperties()` Properties (allocate once, lazily; subsequent calls return the same
  `ObjectRef`). Mirror HotSpot's `ProcessEnvironment.theUnmodifiableEnvironment` /
  `System.props` semantics. Low-risk, self-contained in `native-builtins`.
- B — HANDOFF (VM dispatch owner). The native `java/lang/Object.equals` must NOT shadow a
  subclass bytecode `equals` override: virtual dispatch should resolve the most-derived `equals`
  first and only fall back to the native for true `java.lang.Object` receivers. This touches the
  core invoke/native-shadowing path and overlaps the tracked "Object.toString/equals/hashCode
  intrinsic shadowing" item — needs the dispatch owner, not a local env patch.
- C — FIX, with care. Make the primitive-class resolver authoritative: have
  `int.class`/`Integer.TYPE` resolve to `primitive_class_mirror("int")` directly (or fix the
  synthetic `clinit_*` to write `TYPE` at the REAL resolved field index in real-JDK mode, and
  suppress the synthetic clinit when the real `Integer.<clinit>` runs). Remove/repair the
  wrapper-mirror fallback in `phases_late.rs:39586`. Verify against the kotlin-metadata
  primitive-array path noted in memory to avoid regressing it.
- D — HANDOFF (classloading/resource owner). Make `ClassLoader.getResourceAsStream` honour the
  receiver loader's delegation (custom loader → parent chain → global classpath), or ensure the
  native fires for user-defined subclasses and resolves via the global classpath as a floor.
  Cross-cuts the classloader/resource model; not a localized fix.

## Open questions

1. C: in real-JDK mode, does the synthetic `Integer.<clinit>` (phases_early.rs:1906) actually
   shadow the real `Integer.<clinit>`, or is per-class `<clinit>` shadowing suppressed for
   real-java-home classes (as serialization natives are)? If suppressed, the defect is purely the
   `phases_late.rs:39586` wrapper-mirror fallback firing because `resolve_field_index("Integer","TYPE")`
   or the real `TYPE` value is wrong. Needs a single `int.class` / `int.class==Integer.TYPE`
   probe under `--java-home` (not runnable now: disk 100% full, suite in progress).
2. D: does CratonVM's invoke path fire the `ClassLoader.getResourceAsStream` native for a
   user-defined subclass receiver, or run the real bytecode (`getResource`→parent chain)? That
   determines whether the fix is "make the native receiver-aware" vs. "wire user-defined-loader
   parent delegation to the global classpath." A probe with a trivial anonymous `ClassLoader`
   subclass would disambiguate.
3. A: beyond identity, does the synthetic `System.getProperties()` honour `put(Object,Object)`
   with non-String keys/values and `stringPropertyNames()` filtering (the rest of
   `getSystemProperties()` after the `isSameAs`)? If not, fixing identity alone may surface a
   second assertion failure in that test.
