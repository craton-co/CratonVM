# Bug 06 — Reflection mirror arrays not GC-rooted while built (the WildFly "intermittent" cluster)

**Severity:** High — **CratonVM-only**. A GC that runs *while a reflective
`Field[]`/`Method[]`/`Constructor[]`/`Class[]`/annotation array is being filled*
reclaims/relocates the half-built array (held only in a Rust local, invisible to
the GC root scan). The stale reference then resolves to a reused slot — usually a
bare `java/lang/Object` — corrupting reflection results.

**Status: FIXED** (main checkout `C:\craton\cratonvm`, branch `dev`,
`native-builtins/src/lang_class.rs`). Verified by deterministic standalone repros
under GC stress (== HotSpot after fix) and the full WildFly clustering cluster
(VM corruption → benign no-container FAIL, matching HotSpot).

## This is the real WildFly JUnit-platform cluster
The genuinely-CratonVM-specific B-on failures — `PreconditionViolationException:
annotationType must not be null` ×5, `AbstractMethodError …getId/…getParent/
…hasGenericInformation` ×3, `ClassCastException: java/lang/Object cannot be cast to
…TestExecutionResult$Status` ×1 — are all this one bug. JUnit discovery is
reflection-heavy (`ReflectionUtils.streamFields`/`findAllFieldsInHierarchy`,
`AnnotationSupport.findAnnotation`), so a mid-build GC corrupts a `Field[]`/
annotation array; the corruption surfaces downstream as a null member, a wrong-class
receiver, or a failed array cast depending on where the reused slot lands.

It was first mistaken for a layer-B (`CRATONVM_JIT_VIRTUAL_TIERUP`) bug because B-on
*exposes* it: JIT-compiling the instance-method discovery path raises allocation/GC
pressure during reflection, so the mid-build GC window is hit far more often. The
underlying bug is **not** JIT-specific — it reproduces with the JIT fully disabled
under GC stress (see below). (Distinct from the exception-handler bug
[bug-05](bug-05-jit-exception-handler-this-null.md).)

## Root cause (confirmed)
`native-builtins/src/lang_class.rs`, the reflection array builders, e.g.
`getDeclaredFields0`:
```rust
let arr = ctx.new_ref_array(ClassId::new(0), selected.len());
for (i, meta) in selected.iter().enumerate() {
    let field_obj = create_field_object(ctx, meta); // ALLOCATES → may young-GC
    ctx.set_array_element(arr, i, ...);              // writes to a now-stale `arr`
}
```
`arr` lives only in a Rust local. `create_field_object` (and
`create_method_object`/`create_constructor_object`/`create_annotation_proxy`/
`descriptor_to_class_mirror`) allocate heavily, so a young GC during the loop
relocates/reclaims `arr` — and the already-stored elements — leaving the raw
`ObjectRef` stale (it then resolves to a reused, usually `java/lang/Object`, slot).
The `NativeContext::pin_native_root` doc describes this exact failure mode. A plain
bytecode-allocated array (`new String[]`) or a `java.lang.reflect.Array.newInstance`
result is fine because nothing allocates between its creation and use — only the
*fill-with-allocation* builders are affected.

## Minimal standalone repros (no JIT, no WildFly)
Under `CRATONVM_DBG_GC_STRESS=65536` (force a young GC every 64 KB):
- `repro/MinRepro.java`: `Field[] f = k.getDeclaredFields()` held across an alloc —
  Case A (native reflection) **crashed** `ClassCastException: Object → [Field`;
  Case B (plain `new String[]`) **passed**. → after fix: both pass.
- `repro/ArrRepro.java`: `Array.newInstance` array — **passed** even before the fix
  (control: not a fill-with-allocation builder).
- `repro/ReflRepro.java` / `repro/JUnitReflRepro.java`: getDeclaredFields loop /
  real `ReflectionSupport.findFields`+`AnnotationSupport.findAnnotation` — **crashed**
  before, **pass** after.

| | before fix | after fix |
|---|---|---|
| HotSpot JDK 25 | ok | ok |
| CratonVM, GC_STRESS, JIT-off | **ClassCast / corruption** | ok ✓ |
| CratonVM, B-on (no stress), warmed WildFly batch | annotationType / getName-on-Object / ClassCast | benign `LifecycleException: Could not start container` (== HotSpot) ✓ |

## Fix
A GC-safe array builder, `build_mirror_array` / `build_mirror_array_comp`
(`lang_class.rs`): allocate the array, **pin it as a native GC root**
(`pin_native_root`) across the fill loop, and re-read the forwarded reference
(`read_native_pin`) before each store; already-stored elements stay reachable +
remapped through the pinned array. Applied to all the reflection mirror-array
builders: `getDeclaredFields0` / `getDeclaredMethods0` / `getDeclaredConstructors0`,
the `param`/`exception` arrays inside `create_method_object` /
`create_constructor_object`, `build_annotation_array` /
`build_class_annotation_array`, `getInterfaces` (class + the BeanContainer path),
and `getAnnotationsByType` (class + method).

## Verification
- All standalone repros above: corruption → clean (== HotSpot) under
  `GC_STRESS=65536` and `16384`, JIT-off and B-on.
- WildFly clustering warmed batch (the reliable bug-6 trigger): **0** VM-bug
  signatures (was annotationType ×, getName-on-Object ×27, length-null, ClassCast);
  every class now fails with the benign `LifecycleException: Could not start
  container` → `WFLYLNCHR0001: path 'null'`, matching HotSpot.

## Residual (same pattern, not yet converted; lower-frequency, not in the cluster)
A few reflection builders still fill an array (or a `Vec` of freshly-allocated
mirrors) with allocation without pinning and should be migrated to
`build_mirror_array`: `collect_public_fields` / `collect_public_methods` (the
`getFields`/`getMethods` hierarchy walk — a `Vec`-of-objects build, needs
restructuring to build straight into a pinned array), the per-parameter annotation
arrays (`getParameterAnnotations`), the annotation member name/value arrays inside
`create_annotation_proxy`, and the enum/type-variable array builders. None are on
the verified WildFly cluster path; tracked for a follow-up sweep.
