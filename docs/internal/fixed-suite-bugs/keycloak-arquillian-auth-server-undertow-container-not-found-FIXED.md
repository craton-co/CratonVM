# Keycloak Arquillian `auth-server-undertow` container bootstrap - FIXED

Status: FIXED (2026-07-15). Moved from `docs/known-issues/` after direct
HotSpot and CratonVM verification of the custom suite runner.

## Original symptom

Classes under `testsuite/integration-arquillian/tests/base` (and legacy SSSD
leaves) failed before executing a test with:

```text
java.lang.IllegalStateException: Not found frontend container: auth-server-undertow
```

The failure reproduced byte-for-byte on HotSpot and CratonVM. It was therefore
a runner/bootstrap omission, not a VM defect.

## Root cause and fix

The per-class KcRunner path launched JUnit directly. Unlike Maven Surefire, it
did not supply the transformed module `target/dependency/arquillian.xml` or
its resolved Surefire `systemPropertyVariables`. Arquillian consequently built
a registry without the configured frontend container.

`run-keycloak-suite.ps1` now detects Arquillian integration modules and:

1. requires the generated descriptor (with an actionable build error if absent);
2. materializes the effective module Maven POM, cached under
   `.suite/arquillian-bootstrap`;
3. forwards the resolved Surefire system properties along with
   `-Darquillian.xml`; and
4. retries effective-POM generation from a leaf module directory for legacy
   aggregator-only modules such as `other/sssd`.

## Verification

The exact focused class was launched through the custom runner on the Azure
host after compiling the Keycloak base module:

```text
testsuite/integration-arquillian/tests/base
org.keycloak.testsuite.federation.ldap.UserFederationLdapConnectionTest
```

- HotSpot JDK 17: `PASS`, 5 tests successful, 3 containers successful, and
  `containersFailed=0`.
- CratonVM JDK 25: the old container lookup is gone; the runner discovers and
  starts all three containers, reaches the embedded Keycloak server bootstrap,
  and fails later in an independent Protostream parser residual. No
  `Not found frontend container: auth-server-undertow` signature remains.

The runner script also passes PowerShell parser validation and `git diff --check`.

## Repro

Build test resources first, then run a focused class list:

```powershell
mvn -pl testsuite/integration-arquillian/tests/base -am -DskipTests test-compile
powershell -NoProfile -ExecutionPolicy Bypass -File apps\keycloak-suite-runner\run-keycloak-suite.ps1 `
  -Vm hotspot -Jit on -ClassList <class-list.tsv> -KeycloakRoot apps\keycloak
```

Use `-Vm craton -Exe <unique-cratonvm-binary>` for the corresponding CratonVM
probe.
