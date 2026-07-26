# Keycloak Arquillian Package.getAnnotation resolves to String.getAnnotation

Status: FIXED (branch `fix/package-versioninfo-getannotation-corruption`)

Date observed: 2026-07-03
Date fixed: 2026-07-03

## Summary

The `craton-nonpassed-dev-20260703-01` rerun moved the legacy Arquillian
Keycloak classes past the earlier missing `Arquillian.<init>(Class)` classpath
crash, but 543 classes still failed during Arquillian adaptor bootstrap.

The signature was a CratonVM linkage failure while Arquillian Reporting
initializes MOXy/JAXB and asks for package annotations:

```text
NoSuchMethodError
method="java/lang/String.getAnnotation(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;"
caller="java/lang/Package.getAnnotation(Ljava/lang/Class;)Ljava/lang/annotation/Annotation; @pc=8"
```

JUnit reported this as:

```text
java.lang.RuntimeException: Could not create new instance of class
org.jboss.arquillian.test.impl.EventTestRunnerAdaptor
```

The test process then exited through `System.exit(1)` before any test method
started.

## Evidence

Run:

```text
craton-nonpassed-dev-20260703-01 / others-jit
```

Result file:

```text
C:\craton\CratonVM-keycloak-nonpassed-rerun-20260703-01\apps\keycloak-suite-runner\.suite\results\craton-nonpassed-dev-20260703-01\others-jit\results.tsv
```

Affected rows:

```text
testsuite/integration-arquillian/tests/base        541
testsuite/integration-arquillian/tests/other/sssd    2
total                                             543
```

Representative stdout stack:

```text
Caused by: java.lang.NoSuchMethodError:
java/lang/String.getAnnotation(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;
  java.lang.Package.getAnnotation(Package.java:446)
  java.lang.reflect.AnnotatedElement.isAnnotationPresent(AnnotatedElement.java:291)
  java.lang.Package.isAnnotationPresent(Package.java:456)
  org.eclipse.persistence.jaxb.javamodel.reflection.AnnotationHelper.isAnnotationPresent
  org.eclipse.persistence.jaxb.javamodel.reflection.JavaPackageImpl.getAnnotation
  org.eclipse.persistence.jaxb.compiler.AnnotationsProcessor.getPackageInfoForPackage
  org.eclipse.persistence.jaxb.JAXBContextFactory.createContext
  org.arquillian.recorder.reporter.JAXBContextFactory.initContext
  org.arquillian.recorder.reporter.exporter.ExporterRegistrationHandler.getContext
  org.jboss.arquillian.test.impl.EventTestRunnerAdaptor.<init>
```

## Root cause

`java.lang.Package` in real JDK 9+ (confirmed via `javap -p -c` against the
bundled JDK 25) is NOT the flat 7-string-field layout CratonVM's
`native_class_get_package` assumed. The real layout is:

```text
java.lang.NamedPackage (superclass): slot 0 = name, slot 1 = module
java.lang.Package (own fields):      slot 2 = versionInfo (Package$VersionInfo)
                                      slot 3 = packageInfo (Class<?>)
```

The `Specification-Title`/`-Version`/`-Vendor` and `Implementation-Title`/
`-Version`/`-Vendor` manifest attributes are NOT direct `Package` fields —
they live inside the nested `Package$VersionInfo` record.

`native_class_get_package` (and its sibling `i2_classloader_define_package_class`,
used by `ClassLoader.definePackage(Class)`) wrote these six manifest
attributes by **raw slot index** (1 through 6), on the mistaken assumption
that `Package` declares six flat fields there. Under the real layout, slot 3
is actually `packageInfo`. The `specVendor` write (`write_optional(ctx, 3,
"specVendor", spec_vendor)`) therefore overwrote `packageInfo` with a
`String` — the jar's `Specification-Vendor` manifest value — whenever that
attribute was present (common for JBoss-vendored jars, e.g.
`Specification-Vendor: JBoss by Red Hat`).

Real `Package.getPackageInfo()` only lazily resolves `packageInfo` when the
field is still `null`:

```java
private Class<?> getPackageInfo() {
    if (packageInfo == null) { /* lazy Class.forName(...) */ }
    return packageInfo;
}
```

Once corrupted to a non-null `String`, this lazy path never re-fires.
`Package.getAnnotation()` / `isAnnotationPresent()` then unconditionally
dispatch `getAnnotation(Class)` on `getPackageInfo()`'s return value —
a `String` receiver instead of a `Class` — producing
`NoSuchMethodError: java/lang/String.getAnnotation`.

This matches the observed distribution exactly: only classes from
JBoss-vendored jars (which set `Specification-Vendor`) triggered the crash;
this is also why the corruption survived so long — `getSpecificationTitle()`
and friends (which would have surfaced the same class of bug earlier, just
as an NPE/NoSuchMethodError on `versionInfo` instead of `packageInfo`) are
rarely called directly by application code.

## Fix

`../../../../native-builtins/src/lang_class.rs`:

- `native_class_get_package` and `i2_classloader_define_package_class` no
  longer write the six manifest attributes by raw slot index — only by
  field name (`set_field_by_name`), which correctly no-ops when the named
  field doesn't exist on the real class rather than silently landing on
  whatever field happens to occupy that slot.
- Both functions now wire `versionInfo` to the real
  `Package$VersionInfo.NULL_VERSION_INFO` static sentinel (read via
  `ensure_class_initialized` + `static_field_index_by_name` +
  `get_static_field`), matching what the real `Package(String, Module)`
  constructor does unconditionally. Without this, `versionInfo` stays raw
  `null` on our hand-built object and `getSpecificationTitle()` / `isSealed()`
  (which deref `versionInfo` unconditionally) would NPE instead of
  returning HotSpot's null/false.

Known residual: since `Package` has no flat `specTitle`/etc. fields to
target by name, `getSpecificationTitle()` and friends currently return null
even when the manifest has real values (rather than crashing, which is what
the old by-index write did). Properly populating them would require
constructing a real `Package$VersionInfo` via its private `getInstance(...)`
factory, which needs a static-invoke capability this native layer doesn't
currently expose. Not blocking — no known caller depends on non-null
specification/implementation info coming through the real-class `Package`
path today.

Also added two regression tests in `../../../../native-builtins/src/lang_class.rs`
(`t19_h10_get_package_manifest_writes_do_not_corrupt_module_or_package_info`,
`i2_define_package_class_manifest_writes_do_not_corrupt_module_or_package_info`)
that build a real jar with `Specification-Vendor` set and assert the
resulting `Package` object's slots 2-6 are untouched by the manifest-attribute
write path.

## Verification

Built `cratonvm.exe` with the fix and re-ran the exact repro from this doc
plus two more originally-failing classes (one from
`testsuite/integration-arquillian/tests/other/sssd`, one from
`testsuite/integration-arquillian/tests/base`). All three previously hit
`NoSuchMethodError: java/lang/String.getAnnotation`; after the fix, all
three get past `EventTestRunnerAdaptor` construction (MOXy/JAXB bootstrap
succeeds) and instead fail later with
`java.lang.IllegalStateException: Not found frontend container:
auth-server-undertow` — an unrelated Arquillian container-configuration gap
in this runner invocation (no `auth-server-undertow` profile wired up for a
bare per-class run), not a CratonVM correctness bug.

`cargo test -p cratonvm-native-builtins --lib lang_class::tests`: 142
passed, 0 failed (140 pre-existing + 2 new regression tests).

## Repro (for reference)

```powershell
$list = "C:\temp\keycloak-arquillian-package-annotation-one.tsv"
"module`tclass" | Set-Content -Path $list -Encoding ascii
"testsuite/integration-arquillian/tests/base`torg.keycloak.testsuite.account.AccountRestServiceCorsTest" |
  Add-Content -Path $list -Encoding ascii

powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File "C:\craton\CratonVM\apps\keycloak-suite-runner\run-keycloak-suite.ps1" `
  -ClassList $list `
  -Category others `
  -Vm craton `
  -Jit on `
  -Parallel 1 `
  -TimeoutSec 600 `
  -RunName keycloak-arquillian-package-getannotation-repro `
  -KeycloakRoot "C:\craton\CratonVM\apps\keycloak" `
  -WorkDir "C:\craton\CratonVM\apps\keycloak-suite-runner\.suite" `
  -Exe "C:\craton\CratonVM\target\release\cratonvm.exe"
```
