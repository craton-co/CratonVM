# Keycloak Arquillian: missing java/lang/System$1.defineClass

Status: open

Date observed: 2026-07-03

## Summary

After fixing `jdk/internal/misc/PreviewFeatures.isPreviewEnabled()Z`, the
1044-class Keycloak non-passed rerun reached the Arquillian block. All 621
`testsuite/integration-arquillian/tests/base` rows then crashed with the same
JDK-internal method-resolution failure:

```text
NoSuchMethodError method="java/lang/System$1.defineClass(Ljava/lang/ClassLoader;Ljava/lang/String;[BLjava/security/ProtectionDomain;Ljava/lang/String;)Ljava/lang/Class;"
caller="jdk/internal/reflect/ClassDefiner.defineClass(Ljava/lang/String;[BIILjava/lang/ClassLoader;)Ljava/lang/Class; @pc=32"
```

Top-level stderr:

```text
[cratonvm] main-vm run() returned Err: Exception in thread "main" java/lang/NoSuchMethodError: java/lang/System$1.defineClass(Ljava/lang/ClassLoader;Ljava/lang/String;[BLjava/security/ProtectionDomain;Ljava/lang/String;)Ljava/lang/Class;
    at KcRunner.main(KcRunner.java:34)
    at org.junit.platform.launcher.core.SessionPerRequestLauncher.execute(SessionPerRequestLauncher.java:63)
    at org.junit.platform.launcher.core.InternalTestPlan.from(InternalTestPlan.java:33)
    at jdk/internal/reflect/ClassDefiner.defineClass(ClassDefiner.java:66)
```

## Evidence

Run:

```text
craton-azure-nonpassed-dev-20260703-previewfeatures-fixed-01
```

Results:

```text
/home/victor/wt-keycloak-previewfeatures-suite-20260703-01/apps/keycloak-suite-runner/.suite/results/craton-azure-nonpassed-dev-20260703-previewfeatures-fixed-01/others-jit/results.tsv
```

Count:

```text
621 CRASH rows
```

Representative log:

```text
/home/victor/wt-keycloak-previewfeatures-suite-20260703-01/apps/keycloak-suite-runner/.suite/results/craton-azure-nonpassed-dev-20260703-previewfeatures-fixed-01/others-jit/logs/testsuite_integration-arquillian_tests_base.org.keycloak.testsuite.AbstractAuthenticationTest.err.log
```

## Current conclusion

This is a CratonVM real-JDK surface gap exposed after the PreviewFeatures fix.
JDK reflection serialization-constructor generation calls
`jdk.internal.access.JavaLangAccess.defineClass(...)`; the concrete Java access
object is `java/lang/System$1`. CratonVM does not resolve the
`defineClass(ClassLoader,String,byte[],ProtectionDomain,String)` method on that
object, aborting JUnit test-plan construction for every Arquillian base class.

## Next steps

- Inspect JDK 21 `java.lang.System$1` / `JavaLangAccess` bytecode and confirm
  the exact method descriptor expected by `jdk/internal/reflect/ClassDefiner`.
- Add or expose the real-JDK implementation path needed by this
  `JavaLangAccess.defineClass` bridge.
- Add a minimal probe around `ReflectionFactory.newConstructorForSerialization`
  or `ClassDefiner.defineClass` before rerunning the 621 Arquillian base rows.
