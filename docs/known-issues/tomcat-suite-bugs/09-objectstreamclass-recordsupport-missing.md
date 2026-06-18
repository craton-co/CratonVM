# Bug 09 — ObjectStreamClass$RecordSupport missing → NOSUMMARY (VM death)

**Status:** ✅ FIXED — merged into `dev` (fix commit `c66ffc0d`, originally branch
`fix/tomcat-suite-bugs-09-10`). Re-verified 2026-06-15 on `dev`:
`org.apache.catalina.realm.TestGenericPrincipal` = `OK (3 tests)` == HotSpot.
(Any NOSUMMARY status in older suite runs is stale — it predates the merge.)
**Severity:** Medium-High — a `linkage error` kills the VM (NOSUMMARY); affects
any test serializing a record (or a class whose serialization walks records).
**Repro classes:** `org.apache.catalina.realm.TestGenericPrincipal` (was NOSUMMARY,
now PASS). `TestJNDIRealm`'s NOSUMMARY was a *separate* issue (in-memory LDAP
server / networking), NOT this bug — records now deserialize cleanly under it.

## Fix summary

Two layers, all in worktree `CratonVM-tcsuite0910`:

1. **Bridge `MethodHandles.arrayElementGetter`/`arrayElementSetter` in real-JDK
   mode.** They were registered only in synthetic mode
   (`register_p65_method_handles_extra`), so in real-JDK mode the genuine
   `MethodHandleImpl.makeArrayElementAccessor` bytecode ran and bottomed out in
   the intrinsic / `viewAsType` → `MethodHandle.copyWith` LambdaForm machinery
   (`copyWith` is abstract → "has no Code attribute" AbstractMethodError; for
   `Object[]` it first NPE'd on `MethodTypeForm.cachedLambdaForm`'s null
   `lambdaForms`). `findStatic`/`findVirtual` already work via the real
   DirectMethodHandle path, so only these two factories needed bridging. New
   shared helper `register_array_element_accessor_bridges` (lang_invoke.rs),
   promoted into real-JDK essentials (lib.rs) + a `check_override` allow-list
   entry (vm_exec.rs) so the native wins over the broken bytecode. This alone
   clears the `RecordSupport` *linkage error* (clinit completes).

2. **Real java.io record deserialization.** With (1), deser progressed into
   `ObjectInputStream.readRecord` → `RecordSupport.deserializationCtr`, which the
   JDK assembles from `foldArguments`/`insertArguments`/`arrayElementGetter`
   combinators CratonVM's synthetic MethodHandles cannot execute (it threw
   `IllegalArgumentException` at `insertArguments`). Intercept
   `deserializationCtr` to return a synthetic `MH_KIND_RECORD_DESER` handle
   carrying the record class + `ObjectStreamClass`; its `mh_dispatch` arm, on
   `invokeExact(byte[] primValues, Object[] objValues)` from `readRecord`,
   rebuilds the record reflectively — maps each canonical record component (from
   `Class.getRecordComponents`) to its stream field **by name** (the stream
   reorders fields, primitives-first), pulls reference values from `objValues`
   and decodes primitive values big-endian out of `primValues`, then invokes the
   canonical constructor. New native + dispatch arm + decode helpers
   (lang_invoke.rs), real-JDK essential registration (lib.rs) + a `check_override`
   entry (vm_exec.rs).

Verified: `TestGenericPrincipal` OK(3); a `record Point(int,int)` round-trips
(primitive-component path); non-record serialization unaffected; MethodHandle
regression probe green (`findStatic`/`findVirtual`/`bindTo`); EL record/MH tests
(`TestRecordELResolver`, `TestMethodReference`, …) still pass. The fix is additive
(new MH kind + 2 allow-list entries + essential bridge registrations) — no JIT/GC
paths touched.

## Original diagnosis (for reference)

## Symptom

```
... wrapping in ExceptionInInitializerError
    class=java/lang/invoke/MethodHandleImpl$ArrayAccessor
    cause=java.lang.NullPointerException: Cannot load from null array
Error in thread "main" linkage error: no class def found:
    java/io/ObjectStreamClass$RecordSupport
```

The VM dies (no JUnit summary). `TestGenericPrincipal` serializes a
`GenericPrincipal` via Java serialization; the path reaches
`java.io.ObjectStreamClass`'s record handling and needs the nested
`ObjectStreamClass$RecordSupport`, which CratonVM fails to load/resolve.

## Root cause (hypothesis)

Two related defects:
1. **`java/io/ObjectStreamClass$RecordSupport` not resolvable** — the
   record-serialization helper class is missing from CratonVM's boot view (or its
   resolution fails), so any serialization that consults record canonical-ctor
   support throws `NoClassDefFoundError`/linkage error. Real-JDK mode should load
   it from the JDK image.
2. **`MethodHandleImpl$ArrayAccessor` clinit NPE: "Cannot load from null array"**
   — a `MethodHandle` array-accessor (`MethodHandles.arrayElementGetter`-style)
   initializer dereferences a null array during `<clinit>`. This precedes the
   linkage error and may be the underlying reason `RecordSupport` (which builds
   array-based MethodHandles for record components) can't initialize.

## Next steps

- Confirm whether `java/io/ObjectStreamClass$RecordSupport` exists in the JDK 25
  image and why CratonVM can't def it (class-loader / nestmate / inner-class
  resolution).
- Investigate `MethodHandleImpl$ArrayAccessor` `<clinit>` — the "Cannot load from
  null array" suggests a null static array the accessor table is built from
  (a CratonVM MethodHandle-bootstrap gap).
- These likely share a root in MethodHandle/records support.

## Reproduction

```
cratonvm.exe -Xmx2g -cp <cp> org.junit.runner.JUnitCore \
  org.apache.catalina.realm.TestGenericPrincipal   # CWD: apps/tomcat
# -> NOSUMMARY (linkage error, VM death); HotSpot: PASS
```
