# `TestClassServerTest.testInvalidPackage` — CratonVM doesn't throw `ClassNotFoundException` for an invalid package name that HotSpot does

Status: open — small, low-severity residual; confirmed CratonVM-specific via fresh HotSpot comparison

Date observed: 2026-07-13 (HotSpot re-baseline against `hotspot-refresh-v2-shard1`, compared against CratonVM's
`nonpassed-before-refresh2-shard{1,2,3,4}` results)

## Summary

`test-framework/remote :: org.keycloak.testframework.remote.runonserver.TestClassServerTest::testInvalidPackage`
fails under CratonVM:

```
=> org.opentest4j.AssertionFailedError: Expected java.lang.ClassNotFoundException to be thrown, but nothing was thrown.
   org.junit.jupiter.api.AssertThrows.assertThrows(AssertThrows.java:74)
```

The test asserts that resolving a class from a deliberately-invalid package name throws
`ClassNotFoundException`; under CratonVM, no exception is thrown at all. This class was already flagged as a
known low-priority residual in the 2026-07-07 investigation pass (not root-caused then). It now has a confirmed
HotSpot-vs-CratonVM comparison: HotSpot passes this test cleanly (fresh, non-stale distribution), CratonVM does
not, confirming this is a genuine (if minor) CratonVM classloading behavior gap rather than a harness artifact.

## Next steps

1. Find the exact "invalid package" input the test uses (`TestClassServerTest` source, `testInvalidPackage`
   method) and trace CratonVM's classloading path for that lookup — likely CratonVM's classloader silently
   returns null/some fallback instead of raising `ClassNotFoundException` for certain malformed package/class
   name inputs.
2. Single test, single method — low priority relative to the other findings from this pass, but cheap to fix
   once located.

## Repro

```
cd C:\craton\CratonVM-keycloak-nonpassed-v2-20260710
$jdk = '"C:\Program Files\Java\jdk-25"'
powershell -NoProfile -ExecutionPolicy Bypass -File apps\keycloak-suite-runner\run-keycloak-suite.ps1 -Vm craton -Jit on -TimeoutSec 60 -Parallel 1 -RunName repro-testclassserver-invalidpackage -ClassList <(printf 'module\tclass\ntest-framework/remote\torg.keycloak.testframework.remote.runonserver.TestClassServerTest\n') -KeycloakRoot apps\keycloak -Exe target\release\cratonvm-nonpassed-v2-refresh2-20260712.exe -JdkHome $jdk
```

## Evidence

CratonVM failure: `apps/keycloak-suite-runner/.suite/results/nonpassed-before-refresh2-shard1/all-jit/logs/test-framework_remote.org.keycloak.testframework.remote.runonserver.TestClassServerTest.out.log`.
Fresh HotSpot PASS: `apps/keycloak-suite-runner/.suite/results/hotspot-refresh-v2-shard1/hotspot-jit/results.tsv`.
