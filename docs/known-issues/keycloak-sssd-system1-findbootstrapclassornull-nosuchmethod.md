# Keycloak SSSD: missing java/lang/System$1.findBootstrapClassOrNull

Status: open

Date observed: 2026-07-03

## Summary

After the PreviewFeatures native fix, the two concrete SSSD rows fail on a
missing JDK-internal `JavaLangAccess` bridge:

```text
NoSuchMethodError method="java/lang/System$1.findBootstrapClassOrNull(Ljava/lang/String;)Ljava/lang/Class;"
caller="jdk/internal/loader/BootLoader.loadClassOrNull(Ljava/lang/String;)Ljava/lang/Class; @pc=9"
```

The runner records these as `FAIL` rows because the process exits via
`System.exit(1)` instead of a direct CratonVM abort.

## Evidence

Run:

```text
craton-azure-nonpassed-dev-20260703-previewfeatures-fixed-01
```

Affected rows:

```text
testsuite/integration-arquillian/tests/other/sssd org.keycloak.testsuite.sssd.SSSDTest
testsuite/integration-arquillian/tests/other/sssd org.keycloak.testsuite.sssd.SSSDUserProfileTest
```

Representative log:

```text
/home/victor/wt-keycloak-previewfeatures-suite-20260703-01/apps/keycloak-suite-runner/.suite/results/craton-azure-nonpassed-dev-20260703-previewfeatures-fixed-01/others-jit/logs/testsuite_integration-arquillian_tests_other_sssd.org.keycloak.testsuite.sssd.SSSDTest.err.log
```

## Current conclusion

This is the same general JDK-internal access surface as the larger
`System$1.defineClass` Arquillian bug, but it is a separate method:
`findBootstrapClassOrNull(String)Class`, reached from
`jdk.internal.loader.BootLoader.loadClassOrNull`.

## Next steps

- Audit CratonVM's real-JDK support for `jdk.internal.access.JavaLangAccess`
  methods implemented by `java/lang/System$1`.
- Add focused coverage for `BootLoader.loadClassOrNull`.
- Rerun the three SSSD rows after the bridge method is implemented.
