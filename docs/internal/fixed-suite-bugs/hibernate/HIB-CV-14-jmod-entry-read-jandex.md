# HIB-CV-14 — Reading a class entry from a `.jmod` archive fails (`FileNotFoundException`) → Jandex indexing fails

**Severity:** Low/Medium — fails ≥2 `boot.models` tests that build a Jandex index from the JDK.
**Status:** ✅ FIXED (worktree `fix/hibernate-full-suite`, `native-builtins/src/net_phase_e.rs`). `URL.openStream` on a `.jmod` URL now resolves entries under the `classes/` prefix. Verified `SourceModelTestHelperSmokeTests` ok=1.
**Mode:** Interpreter (JIT-off census) — not a JIT bug.
**HotSpot:** not affected.

## Symptom

```
org.hibernate.models.jandex.internal.JandexIndexerHelper$JandexIndexingException: Error indexing standard types
Caused by: java.io.FileNotFoundException: entry java/lang/Object.class in
  C:/Program Files/Java/jdk-25/jmods/java.base.jmod: specified file not found in archive
   at org.hibernate.orm.test.boot.models.SourceModelTestHelper.buildJandexIndex(SourceModelTestHelper.java:128)
```

Affected (sample): `SourceModelTestHelperSmokeTests`, `SimpleAnnotationUsageTests`.

## Root cause (mechanism)

`SourceModelTestHelper.buildJandexIndex` indexes the JDK baseline types by opening the JDK module
archive `…/jmods/java.base.jmod` and reading class entries such as `java/lang/Object.class`. A
`.jmod` file is a ZIP archive whose class entries are stored under a **`classes/`** prefix (i.e. the
real entry is `classes/java/lang/Object.class`). CratonVM's `.jmod`/zip entry lookup does not resolve
`java/lang/Object.class` to that prefixed entry, so it reports `FileNotFoundException: … specified
file not found in archive`. HotSpot's `JarFile`/zip handling for `.jmod` resolves the entry.

## Suspected area / next step

CratonVM's zip/jar/jmod entry reading (native-io / zip natives). Confirm whether the failure is the
missing `classes/` prefix handling for `.jmod` archives or a more general zip central-directory
lookup gap. Minimal repro: open `java.base.jmod` as a `java.util.zip.ZipFile` and
`getEntry("classes/java/lang/Object.class")` vs `getEntry("java/lang/Object.class")`, comparing
CratonVM vs HotSpot.
