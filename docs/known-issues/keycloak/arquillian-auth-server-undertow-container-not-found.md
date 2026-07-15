# Arquillian `testsuite/integration-arquillian` classes fail with "Not found frontend container: auth-server-undertow" — confirmed NOT a CratonVM bug (reproduces identically under real HotSpot)

Status: confirmed non-CratonVM harness/environment gap — verified via direct HotSpot A/B comparison in this
exact custom harness, not just cited from an older rollup note

Date verified: 2026-07-15 (branch fix/keycloak-nonpassed-rerun-v2-20260710)

## Summary

~277-543 `testsuite/integration-arquillian/tests/base` (+ a couple `tests/other/sssd`) classes fail every test at
suite-init time with:

```
=> java.lang.IllegalStateException: Not found frontend container: auth-server-undertow
   org.keycloak.testsuite.arquillian.AuthServerTestEnricher.initializeSuiteContext(AuthServerTestEnricher.java:233)
```

## Verification

Ran the exact same class (`org.keycloak.testsuite.federation.ldap.UserFederationLdapConnectionTest`) through the
identical harness (`run-keycloak-suite.ps1`), same JDK, same checkout, under `-Vm hotspot` instead of
`-Vm craton`:

```
KCRUNNER_RESULT tests=0 failed=0 aborted=0 skipped=0 containersFailed=1
=> java.lang.IllegalStateException: Not found frontend container: auth-server-undertow
   org.keycloak.testsuite.arquillian.AuthServerTestEnricher.lambda$initializeSuiteContext$6(AuthServerTestEnricher.java:233)
   java.base/java.util.Optional.orElseThrow(Optional.java:403)
   ...
   org.jboss.arquillian.junit.Arquillian.run(Arquillian.java:103)
```

Byte-for-byte identical exception, message, and throw site under real HotSpot (JDK 25). This is a clean,
direct A/B match — not an inference from an older/stale baseline.

## Root cause (harness-level, not CratonVM)

`AuthServerTestEnricher.initializeSuiteContext()` looks up a container qualifier (`auth-server-undertow`) in
Arquillian's `ContainerRegistry`, populated from `testsuite/integration-arquillian/tests/base/src/test/resources/arquillian.xml`.
That file *does* define `<container qualifier="auth-server-undertow" mode="manual" default="true">` — so the
container is declared — but the registry Arquillian's core bootstrap builds at runtime doesn't contain it when
tests are launched via this project's custom per-class-JVM harness (`run-keycloak-suite.ps1`) rather than a real
`mvn test`/`mvn failsafe:integration-test` invocation. Since the identical gap reproduces under real HotSpot,
this is conclusively a property of *how this harness launches the test* (missing some Maven Surefire/Failsafe-
supplied bootstrap step or system property that a real Maven test run would provide), not a CratonVM defect —
every class under this module would need a fully-configured Arquillian container adapter setup this harness
doesn't replicate, on any JVM.

## Disposition

No CratonVM fix needed. If these Arquillian-based legacy tests are ever wanted for CratonVM bug-hunting, the fix
belongs in the harness (`run-keycloak-suite.ps1` or a real `mvn failsafe` invocation for this specific module),
not in CratonVM. Not investigated further here.

## Repro

```
cd C:\craton\CratonVM-keycloak-nonpassed-v2-20260710
$jdk = '"C:\Program Files\Java\jdk-25"'
# module\tclass
# testsuite/integration-arquillian/tests/base\torg.keycloak.testsuite.federation.ldap.UserFederationLdapConnectionTest
powershell -NoProfile -ExecutionPolicy Bypass -File apps\keycloak-suite-runner\run-keycloak-suite.ps1 -Vm hotspot -Jit on -TimeoutSec 60 -Parallel 1 -RunName repro-arquillian-undertow-hotspot -ClassList <path-to-classlist-above> -KeycloakRoot apps\keycloak -JdkHome $jdk
```
(Swap `-Vm hotspot` for `-Vm craton` with a CratonVM `-Exe` to see the identical failure under CratonVM.)

## Evidence

- CratonVM failures: `apps/keycloak-suite-runner/.suite/results/nonpassed-v3-shard{3,4}/all-jit/logs/testsuite_integration-arquillian_tests_base.*.out.log`
  (2026-07-14 rerun, binary `cratonvm-nonpassed-v3-refresh-20260714.exe`, `dev` commit `e85f76d00`).
- HotSpot A/B repro: `apps/keycloak-suite-runner/.suite/results/repro-arquillian-undertow-hotspot/hotspot-jit/logs/*.out.log`
  (2026-07-15, JDK 25).
- Container defined but not found: `apps/keycloak/testsuite/integration-arquillian/tests/base/src/test/resources/arquillian.xml:100`.
