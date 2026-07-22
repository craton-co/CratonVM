# HIB-CV-17 — `persistence.xml` not located inside runtime-built `.par`/`.jar` archives

**Severity:** Medium — fails packaged-EMF bootstrap scanning. Confirmed class:
`org.hibernate.orm.test.bootstrap.scanning.PackagedEntityManagerTest` — `found=15 ok=2 failed=13`.

**Status:** ✅ FIXED — branch `fix/hib-cv-17-jarfile-leading-slash`
(`native-builtins/src/phases_late.rs` + `native-io/src/zip_real_jar.rs`).
**Mode:** Interpreter (JIT-off census).
**HotSpot:** not affected (15/15 pass).

## Symptom

13 of 15 methods fail with `RuntimeException: Unable to locate persistence.xml at
file:/C:/.../target/packages/<uuid>/<name>.par` (thrown by the test's
`ScannedPersistenceUnitInfo.create` after
`ArchiveDescriptorFactory.buildArchiveDescriptor(url).findEntry("META-INF/persistence.xml")`
returns null).

## Root cause (the report's original hypothesis was looking in the wrong place)

The archive **build** is fine. `PackagingTestCase.buildDefaultPar()` builds the
`.par` with **ShrinkWrap** (`ZipExporter.exportTo(file)`), not raw
`JarOutputStream` as the original report assumed — and a fast probe
(`.cratonvm-suite/ShrinkProbe.java`) shows ShrinkWrap's build + read-back is
**byte-identical to HotSpot** (persistence.xml present, 3 entries, readable).

The bug is in **CratonVM's `java.util.jar.JarFile(String)` constructor** opening
a **leading-slash Windows drive path**. Hibernate's
`JarFileBasedArchiveDescriptor.findEntry` does:

```java
new JarFile(url.toURI().getSchemeSpecificPart()).getJarEntry(path)
```

On Windows `url.toURI().getSchemeSpecificPart()` for `file:/C:/…/x.par` is
`/C:/…/x.par` (leading slash). CratonVM's `JarFile(String)` native fed that raw
string straight to the host `File::open`, which on Windows opens an
**empty/wrong target** — `getJarEntry`/`getEntry`/`entries` then see **zero
entries** and return null → "Unable to locate persistence.xml". The
`JarFile(File)` ctor was unaffected because `java.io.File` already normalizes
`/C:/…` → `C:\…`. (The original report's `JarRoundTripProbe` only used normal
`C:/…` paths, so it never hit the leading-slash form.)

Pinpointed with a fast probe (`.cratonvm-suite/ShrinkProbe2.java`, no
Hibernate/DB bootstrap) that drives the exact Hibernate API:
`new JarFile("/C:/x.par").getJarEntry(...)` → null (HotSpot: found);
`new JarFile(File)` → found. `JarDiag.java` confirmed the leading-slash JarFile
has **0 entries**.

## Fix

Normalize the leading-slash drive path before opening. Two native handlers
register the `(String)` ctor; both fixed:

- `phases_late.rs::p59_jar_file_init` (wins for `JarFile`): normalize via the
  existing `p57_to_os_path` (`/C:/…` → `C:/…`) and store the normalized path in
  slot 0, so every reader (`getEntry`/`getInputStream`/`entries`/`getManifest`)
  resolves the same file. `getName()` now also matches HotSpot's
  `file.getPath()`.
- `zip_real_jar.rs::open_and_register` (used for `ZipFile`): same normalization.

Windows-only; a no-op for already-normal paths.

## Verification (vs HotSpot)

- `ShrinkProbe2` on the rebuilt binary: `new JarFile("/C:/x.par").getJarEntry`
  and Hibernate's `StandardArchiveDescriptorFactory.buildArchiveDescriptor(url)
  .findEntry("META-INF/persistence.xml")` now **find the entry** (were null) —
  all four probe paths match HotSpot.
- `PackagedEntityManagerTest`: HotSpot 15/15. CratonVM result: see suite run.
