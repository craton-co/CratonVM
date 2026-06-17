# bug-05: generic-type reflection throws `FieldTypeSignature cannot be cast to Type[]` (12 classes)

| | |
|---|---|
| **Category** | **VM-CORRECTNESS** (generics reflection) — biggest distinct CV-unique FAIL family |
| **Modules** | spring-beans (core), spring-aop, spring-context (events/config), spring-validation |
| **CratonVM** | `java.lang.ClassCastException: sun/reflect/generics/tree/FieldTypeSignature cannot be cast to [Ljava/lang/reflect/Type;` |
| **HotSpot JDK 25** | OK (e.g. `BeanWrapperGenericsTests` 42/42) |
| **CratonVM HEAD** | `8e8e47d9` (suite run) |
| **Status** | **ROOT-CAUSED + FIX STAGED** on `fix/oom-array-alloc-abend` (building) — deterministic core fixed; GC-race instances fold into bug-04 |
| **Suggested owner** | **me** |

## Blast radius (≥12 distinct test classes)
```
spring-beans:  BeanUtilsTests, BeanWrapperTests, BeanWrapperGenericsTests,
               DirectFieldAccessorTests, CustomEditorTests
spring-beans/factory: BeanConfigurerSupportTests, xml.support.CustomNamespaceHandlerTests
spring-aop:    framework.autoproxy.AutoProxyCreatorTests
spring-context: annotation.ConfigurationWithFactoryBeanAndAutowiringTests,
               annotation.configuration.ConfigurationClassProcessingTests,
               event.ApplicationListenerMethodAdapterTests
spring-validation: DataBinderTests
```
These are *core* beans/binding/event classes, so the true cascade (BeanCreationException etc.) is
likely larger — generic property-type resolution underpins much of the container.

## Symptom
During generic-type resolution (Spring's `GenericTypeResolver` / `ResolvableType` / property-type
introspection), a JDK reflection call that should yield a `java.lang.reflect.Type[]` instead receives
a single `sun.reflect.generics.tree.FieldTypeSignature` node and the `(Type[])` cast fails:
```
java.lang.ClassCastException: sun/reflect/generics/tree/FieldTypeSignature
    cannot be cast to [Ljava/lang/reflect/Type;
```

## Confirmed CV-unique
`BeanWrapperGenericsTests` — **HotSpot: 42/42 OK**; CratonVM: fails with the CCE.

## ⚠️ Two distinct components (isolation testing)
Re-running the affected classes **one per JVM** splits the family in two:
- **Deterministic reifier bug** — `BeanUtilsTests` reproduces the CCE in isolation (6/38 fail). The
  failing tests are `BeanUtils.copyProperties` with **generic type matching through a type hierarchy**:
  `copyPropertiesHonorsGenericTypeMatches{FromWildcardToWildcard,ForUpperBoundedWildcard}`,
  `copyPropertiesWithGenericCglibClass`, `...GenericsInTypeHierarchy...` — i.e. resolving a method's
  type variable `T` against a subclass (`class User extends GenericBaseModel<Integer>`) and wildcard
  bounds. **This is the real, fixable reifier bug.**
- **GC-race component** — `DirectFieldAccessorTests` **passes 93/93 in isolation** but was recorded
  FAIL with the same CCE in the 4-way batch run → that instance is the GC-root-undercount race
  ([[bug-04-string-constant-corrupted-to-object-under-load]] / spring-bug-10) corrupting a synthetic
  generics object under load, **not** a reifier logic bug.

So the "12 classes" headline overstates the deterministic bug: some are GC-race noise. The
deterministic core (BeanUtils-style generic-hierarchy / wildcard resolution) is the part to fix here;
the rest folds into the GC-race work.

## Reproduce
```bash
VM=C:/craton/spring-vm-stable/cratonvm.exe   # 8e8e47d9
JDK='C:\Program Files\Eclipse Adoptium\jdk-25.0.2.10-hotspot'
CP="<harness>;$(tr -d '\r' < .../spring-beans/build/cratonvm-testcp.txt)"
"$VM" --java-home "$JDK" -cp "$CP" KRun org.springframework.beans.BeanWrapperGenericsTests   # CCE
"$JDK\bin\java.exe"      -cp "$CP" KRun org.springframework.beans.BeanWrapperGenericsTests   # OK
```

## ROOT CAUSE — PINPOINTED + FIX STAGED
Deterministic repro (no JUnit): `BeanUtils.copyProperties` of `List<Integer>` → `List<?>` throws the
CCE; the stack is
```
ClassCastException at SerializableTypeWrapper$TypeProxyInvocationHandler.invoke(SerializableTypeWrapper.java:208)
  <- ResolvableType.resolveType <- ... <- ResolvableType.hasUnresolvableGenerics
  <- BeanUtils.isAssignable <- BeanUtils.copyProperties
```
SerializableTypeWrapper invokes a `Type[]`-returning generics method reflectively and does
`@Nullable Type[] result = new Type[((Type[]) returnValue).length];` — a `(Type[]) returnValue` cast.
A direct reflective probe shows the smoking gun: CratonVM returns these arrays typed
**`[Ljava.lang.Object;` (`Object[]`)** where HotSpot returns **`[Ljava.lang.reflect.Type;` (`Type[]`)**:
```
DIRECT/REFLECT  List<Integer>.getActualTypeArguments -> CratonVM [Ljava.lang.Object;   HotSpot [Ljava.lang.reflect.Type;
DIRECT/REFLECT  ?.getUpperBounds                     -> CratonVM [Ljava.lang.Object;   HotSpot [Ljava.lang.reflect.Type;
```
`Object[] cannot be cast to Type[]`. (The `FieldTypeSignature` variant of the message is the same bug
where the leaked element is an unreified tree node.)

**Source:** `native-builtins/src/generics.rs` builds every synthetic generic-reflection array —
`actualTypeArguments`, `TypeVariable.bounds`, wildcard `upperBounds`/`lowerBounds` (in both the
bare-interface `type_sig_to_java` path and the real-`*Impl` `typesig_to_real_type` path) — with
`ctx.new_ref_array(ClassId::new(0) /* java/lang/Object */, …)`, so the runtime array class is
`Object[]`, not `Type[]`.

**Fix:** new `new_type_array(ctx, len)` helper allocates `java/lang/reflect/Type[]`; all 13 generics
array allocations in `generics.rs` now use it. (Type-checks clean; release build + verification of the
`BeanUtils.copyProperties List<Integer>→List<?>` repro and re-run of `BeanUtilsTests` in progress.)

## Earlier lead — `TypeVariableImpl` bounds reification (superseded by the above)
A stack dump from a failing class (`DirectFieldAccessorTests`) lands deepest in:
```
sun/reflect/generics/reflectiveObjects/TypeVariableImpl.make(
    Ljava/lang/reflect/GenericDeclaration; Ljava/lang/String;
    [Lsun/reflect/generics/tree/FieldTypeSignature;        <-- bounds, a FieldTypeSignature[]
    Lsun/reflect/generics/factory/GenericsFactory;) : TypeVariableImpl
```
So the bug is in **type-variable bounds reification**: `TypeVariableImpl` stores its bounds as a
`FieldTypeSignature[]` and reifies them to `Type[]` (in `getBounds()` / via the `GenericsFactory` /
`Reifier`). CratonVM is producing a single `FieldTypeSignature` where the `Type[]` is required → the
`(Type[])` cast fails. The *simple* `TypeVariable.getBounds()` works in a standalone probe, so the
trigger is a type variable resolved **through the factory/scope** (a property/method type that
references a class- or method-level type parameter — exactly Spring's `GenericTypeResolver`/
`ResolvableType` property walk). The `/`-separated CCE names confirm CratonVM raises the bad cast (native
checkcast / synthetic generics object), not interpreted JDK bytecode HotSpot also runs.

> The same dump shows the path was slow enough to trip the 280s watchdog — the synthetic
> generics-reflection reification is a **perf risk** as well as the correctness bug.

Pinpoint: CratonVM's native/synthetic builder for `TypeVariableImpl` (and the `Reifier`/
`CoreReflectionFactory.makeTypeVariable` path) — ensure the `bounds` reification yields a `Type[]`,
and that the synthetic generics objects override `toString` (the related `GenericArrayType@<hash>`
gap).

**Related signal:** the same standalone probe shows CratonVM returns a `GenericArrayType`'s component
as `java.lang.reflect.ParameterizedType@<hash>` (default `Object.toString`) where HotSpot prints
`java.util.List<java.lang.String>` — i.e. CratonVM's generics-reflection objects are **synthetic** and
incompletely model the `sun.reflect.generics` types (missing `toString`, and — for some nested/array
shapes — returning a `FieldTypeSignature` where a reified `Type[]` is required). Pinpoint the exact
`Reifier`/`CoreReflectionFactory` path that yields a single `FieldTypeSignature` instead of a
`Type[]`; the stack from a failing class (capture in progress) names the JDK call.

## Notes
- High blast radius across core beans → fixing this likely clears a large slice of the suite's
  `BeanCreationException`/`MethodInvocationException` cascades too.
- Distinct from the kotlin-reflect generics work (memory `kotlin-reflect-getjavatype-via-getgenericparams`)
  — this is the plain-Java `sun.reflect.generics` reification path.
