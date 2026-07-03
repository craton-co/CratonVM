# Keycloak Arquillian Package.getAnnotation resolves to String.getAnnotation

Status: open

Date observed: 2026-07-03

## Summary

The `craton-nonpassed-dev-20260703-01` rerun moved the legacy Arquillian
Keycloak classes past the earlier missing `Arquillian.<init>(Class)` classpath
crash, but 543 classes still fail during Arquillian adaptor bootstrap.

The current signature is a CratonVM linkage failure while Arquillian Reporting
initializes MOXy/JAXB and asks for package annotations:

```text
NoSuchMethodError
method="java/lang/String.getAnnotation(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;"
caller="java/lang/Package.getAnnotation(Ljava/lang/Class;)Ljava/lang/annotation/Annotation; @pc=8"
```

JUnit reports this as:

```text
java.lang.RuntimeException: Could not create new instance of class
org.jboss.arquillian.test.impl.EventTestRunnerAdaptor
```

The test process then exits through `System.exit(1)` before any test method
starts.

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

Representative stderr log:

```text
C:\craton\CratonVM-keycloak-nonpassed-rerun-20260703-01\apps\keycloak-suite-runner\.suite\results\craton-nonpassed-dev-20260703-01\others-jit\logs\testsuite_integration-arquillian_tests_base.org.keycloak.testsuite.account.AccountR-ab160fe09565.err.log
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

## Repro

Use the module classpath runner against one affected class:

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

## Current assessment

This is distinct from the fixed `Arquillian.<init>(Class)` issue. The module
classpath is now good enough to construct Arquillian and enter the reporting
extension. The failure is inside JDK `Package.getAnnotation(Class)` bytecode:
CratonVM attempts to call `getAnnotation(Class)` on `java.lang.String`, which
does not define that method.

The likely area is CratonVM's synthetic `java.lang.Package` / `packageInfo`
state. `Package.getAnnotation` should query package annotation metadata through
the package-info path; it should not dispatch an `AnnotatedElement` method on a
plain `String`.

This is related to earlier package/JAXB work, but the observed failure is not
the old null-module bug. Here the package annotation path is non-null but points
at the wrong receiver shape.

## Next steps

- Build a small repro around `SomeClass.class.getPackage().getAnnotation(...)`
  for a package with and without `package-info.class`.
- Inspect the synthetic `java.lang.Package` fields CratonVM writes in
  `native_class_get_package`, especially the `packageInfo` receiver used by
  real JDK `Package.getAnnotation` bytecode.
- Compare HotSpot and CratonVM for `pkg.getDeclaredAnnotations()`,
  `pkg.getAnnotation(XmlSchema.class)`, and `pkg.isAnnotationPresent(...)`.
