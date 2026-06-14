# Bug 08 — ImportHandler standard-package class list NPE  (FAIL)

**Status:** OPEN. Needs triage (may be a real gap or test-data/classpath).
**Severity:** Low/Medium. **Repro class:**
`jakarta.el.TestImportHandlerStandardPackages` — `Tests run: 1, Failures: 1`.

## Symptom

```
java.lang.NullPointerException: Cannot invoke get on null
  at jakarta.el.TestImportHandlerStandardPackages.checkPackageClassList(...:58)
  at jakarta.el.TestImportHandlerStandardPackages.testClassListsAreComplete(...:46)
```

## Root cause (hypothesis)

The test verifies that `jakarta.el.ImportHandler`'s built-in standard-package
class lists (`java.lang`, etc.) are complete by comparing them against the set of
classes the JVM reports for those packages. The `get on null` means a lookup
(map/collection) returned null — most likely CratonVM does not expose a
package's class list the way the test enumerates it (e.g. a
`Package`/module-reflection or resource-listing API returning null), so the
test's own map is null when indexed.

This could be:
- a real CratonVM reflection/module gap (package class enumeration), or
- a test that depends on a JDK-internal class list that differs under CratonVM.

Needs a look at the test source (line 46/58) to see which API returns null.

## Next steps

- Read `TestImportHandlerStandardPackages.java:46-58` to identify the null
  source (which package-enumeration / `ImportHandler` API).
- Determine real-gap vs environmental; if real, file against the relevant
  reflection/package native.

## Reproduction

```
cratonvm.exe -Xmx2g -cp <cp> org.junit.runner.JUnitCore \
  jakarta.el.TestImportHandlerStandardPackages   # CWD: apps/tomcat
# -> Tests run: 1, Failures: 1 ; HotSpot: PASS
```
