# Quarkus: `MethodHandles.Lookup.findClass` / `MHFindClassUtil` Discovery Failure

## Status
**OPEN, root-caused.** Discovered during Quarkus test suite failures triage (`run-20260914-052033-passed`).

## Symptom
`ClassLoadingChainAnalyzerTest.analyzeFindsClassesLoadedViaMethodHandlesFindClass()` fails assertion on `MethodHandles.Lookup.findClass`:

```
Failures (1):
  JUnit Jupiter:ClassLoadingChainAnalyzerTest:analyzeFindsClassesLoadedViaMethodHandlesFindClass()
    MethodSource [className = 'io.quarkus.deployment.pkg.steps.ClassLoadingChainAnalyzerTest', methodName = 'analyzeFindsClassesLoadedViaMethodHandlesFindClass', methodParameterTypes = '']
    => org.opentest4j.AssertionFailedError: Should discover Target loaded via MHFindClassUtil ==> expected: <true> but was: <false>
       org.junit.jupiter.api.Assertions.assertTrue(Assertions.java:232)
       io.quarkus.deployment.pkg.steps.ClassLoadingChainAnalyzerTest.assertSeedDiscovery(ClassLoadingChainAnalyzerTest.java:234)
       io.quarkus.deployment.pkg.steps.ClassLoadingChainAnalyzerTest.analyzeFindsClassesLoadedViaMethodHandlesFindClass(ClassLoadingChainAnalyzerTest.java:220)
```

## Root Cause
`MethodHandles.Lookup.findClass(String targetName)` is a Java 9+ method on `java.lang.invoke.MethodHandles.Lookup` used for discovering and linking classes via lookup instances.

In CratonVM's native registration tables (`native-builtins/src/classloader.rs` / `lookup_define.rs`), while `defineClass` and `defineHiddenClass` are native-implemented and registered, `MethodHandles.Lookup.findClass` is left to delegate to JDK bytecode or native fallback.

When `MethodHandles.Lookup.findClass` runs, access checking or lookup class / ClassLoader namespace propagation fails to properly register/resolve the target class within the `ClassLoadingChainAnalyzer`'s recording context (or fails caller-frame access check), causing the loaded class discovery check to return `false`.

## Affected Tests / Scenarios
- `io.quarkus.deployment.pkg.steps.ClassLoadingChainAnalyzerTest.analyzeFindsClassesLoadedViaMethodHandlesFindClass`

## Remediation / Solution Plan
1. Audit `MethodHandles.Lookup.findClass` handling in `native-builtins/src/lang_invoke.rs` and `lookup_define.rs`.
2. Ensure caller frame resolution and ClassLoader delegation in `MethodHandles.Lookup.findClass` match HotSpot specifications (JEP 259 / Java 9+ MethodHandles access rules).
