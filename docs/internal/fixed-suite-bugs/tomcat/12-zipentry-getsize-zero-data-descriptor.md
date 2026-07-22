# Tomcat suite bug 12 — `JarEntry.getSize()` returns 0 (synthetic slot layout vs real ZipEntry fields)

**Status:** FIXED
**Severity:** broke JSTL/JSP tests that load classes from a webapp `WEB-INF/lib` JAR
(e.g. `jakarta.el.TestCompositeELResolver`), and the same defect underlay the Quarkus
`RunnerClassLoader` 0-length-class failures.

## Symptom

```
ClassLoader.defineClass1(org/apache/taglibs/standard/tlv/JstlCoreTLV) failed:
  ClassFormatError "class file too short (0 bytes; need at least 8 for header)"
```

The webapp class loader sizes its read buffer from `JarEntry.getSize()` (== 0) and calls
`defineClass` with 0 bytes.

## Root cause

`JarFile.getEntry`/`getJarEntry`/`entries()` are served in real-JDK mode by
`p59_jar_lookup_entry` / `p59_jar_collect_entries` (`native-builtins/src/phases_late.rs`).
They allocate a **real-layout** `java/util/jar/JarEntry` via
`alloc_concurrent_synthetic(... "JarEntry", 4)` then wrote the values to the **legacy
synthetic 4-slot layout** (`0=name, 1=size, 2=csize, 3=method`). The real `ZipEntry`
field order is `name, xdostime, crc, size, csize, method, …`, so those writes landed in
the WRONG fields (`size`→`xdostime`, `csize`→`crc`, `method`→`size`), leaving the real
`size`/`csize`/`method`/`crc` at their defaults. The inherited real `ZipEntry.getSize()`
reads the real `size` field → **0**. (Confirmed with a `CRATONVM_DBG_ZIPFIELD` field-write
tracer: the native wrote slot 0=name, slot 1=`Long(1799)`, slot 2=`Long(839)`,
slot 3=`Int(8)` — correct values, wrong slots — while `getSize()` returned 0.)

## Fix

Write the real `ZipEntry` fields **by name** (`name`/`size`/`csize`/`method`/`crc`) in
addition to the legacy slots (kept for synthetic-jdk readers; `set_field_by_name` no-ops
if the field is absent), and capture `crc`.

- `p59_jar_lookup_entry` (getEntry/getJarEntry): fixed earlier for Quarkus
  `RunnerClassLoader`.
- `p59_jar_collect_entries` (`entries()`): fixed here — completes the same defect for the
  enumeration path (JSTL TLV loading walks entries).

Verified vs HotSpot on the real taglibs jar: `size=1799 csize=839 crc=835568a method=8`
(was 0/0/0/0); junit data-descriptor `MANIFEST.MF` size=321 (was 0);
`TestWebappClassLoader`/`TestStandardService` still green.

## Follow-on (separate bug)

With the size fix, `JstlCoreTLV` loads with correct bytes, but the test then hits a
**distinct** classloader bug: resolving its superclass `JstlBaseTLV` during `defineClass`
fails with `ClassNotFound`, even though `JstlBaseTLV.class` is present in the same jar and
`getJarEntry(...).getSize()` reads it correctly (7952). Webapp-classloader
superclass-resolution issue — file separately (bug 13).

## How it was found

Env-trace probes in all invoke dispatchers + `safe_native_call` could not see the builder
(the `p59_*` natives run with an empty Java frame stack, off the instrumented paths). A
field-write tracer (`CRATONVM_DBG_ZIPFIELD`) on the `Putfield` opcode handler and the
native `set_field`/`set_field_by_name` impls — dumping class + slot + value — immediately
revealed the synthetic-slot mis-store. Repro assets: `apps/tomcat` +
`scratch/jar/JarRead3.java`, `scratch/jar/BB.java`, `scratch/ziptest/` (in the tcfull
worktree).
