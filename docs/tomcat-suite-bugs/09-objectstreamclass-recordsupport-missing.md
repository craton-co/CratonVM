# Bug 09 — ObjectStreamClass$RecordSupport missing → NOSUMMARY (VM death)

**Status:** OPEN. Real CratonVM bug (HotSpot PASSes).
**Severity:** Medium-High — a `linkage error` kills the VM (NOSUMMARY); affects
any test serializing a record (or a class whose serialization walks records).
**Repro classes:** `org.apache.catalina.realm.TestGenericPrincipal` (NOSUMMARY),
likely also `TestJNDIRealm` (NOSUMMARY).

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
