# `Properties.computeIfAbsent` null-map NPE in Spring Boot `MapBinderTests`

**Status: FIXED and retired 2026-07-28**

## Symptom

Spring Boot's `MapBinderTests.bindToPropertiesShouldBeEquivalentToMapOfStringString()`
previously failed while binding into a `java.util.Properties` target:

```
java.lang.NullPointerException: Cannot invoke
"java.util.concurrent.ConcurrentHashMap.computeIfAbsent(...)" because "this.map" is null
    java.util.Properties.computeIfAbsent(Properties.java:1496)
    org.springframework.boot.context.properties.bind.MapBinder$EntryBinder.bindEntries(MapBinder.java:186)
```

The failure was a real-JDK 25 semantic mismatch, not a Spring binder defect.
CratonVM intentionally represents several `Properties` instances, including
synthetic/system instances, through its GC-aware properties side table. Those
objects do not populate JDK 25 `Properties`' private `ConcurrentHashMap map`
field, so allowing real `Properties.computeIfAbsent` bytecode to run dereferenced
the null field.

## Repair

Commit `2a44df5cce7d5a3446c45a05c341005b8283b6ef` added the permanent bridge
`native_properties_compute_if_absent` in
`native-builtins/src/properties_sidetable.rs` and registered it for:

```
java/util/Properties.computeIfAbsent
    (Ljava/lang/Object;Ljava/util/function/Function;)Ljava/lang/Object;
```

The bridge has the same side-table semantics as `Properties.get` and `put`:
it returns an existing value without invoking the mapper, invokes the mapper
only for an absent key, stores a non-null mapper result, and returns null
without storing when the mapper result is null. It pins and rereads `this`,
the key, the mapper, and an object mapper result around every allocating or
Java-reentrant operation, preserving correctness with the moving collector.

This is intentionally a targeted `Properties` bridge rather than an attempt to
manufacture the private JDK `ConcurrentHashMap` layout in synthetic objects.
The latter would split storage between real JDK internals and the established
GC-aware side table.

## Closure validation

The affected class is the complete 45-test
`core/spring-boot` `MapBinderTests` class. A fresh fat-LTO release build
(29m13s) produced:

```
cratonvm-properties-map-20260728-019fa8dd.exe
SHA-256 EF93787280EA0D66DAB6010B1AD9197939228EF63C52B979D8FCB4B35A334684
```

After merging current `origin/dev`, the final integrated release executable
had SHA-256 `C61F1E896735B06E22FBD7CA34624ACA3CE53B5B93837D6CADB08549D53721E7`.
Using the external `apps/spring-boot` fixture with JDK 25 and the suite
runner's per-module generated classpath, the complete class passed in both
required modes:

| Mode | `SBRUNNER_RESULT` accounting | Wall test time |
|---|---|---:|
| JIT | `tests=45 failed=0 aborted=0 skipped=0 containersFailed=0` | 11.2s |
| `--nojit` | `tests=45 failed=0 aborted=0 skipped=0 containersFailed=0` | 12.4s |

This validates the former `bindToPropertiesShouldBeEquivalentToMapOfStringString`
failure together with the other 44 `MapBinderTests` methods in both execution
modes.

## Affected scope

| Module | Class | Result |
|---|---|---|
| `core/spring-boot` | `org.springframework.boot.context.properties.bind.MapBinderTests` | retired after complete class-level JIT and `--nojit` validation |
