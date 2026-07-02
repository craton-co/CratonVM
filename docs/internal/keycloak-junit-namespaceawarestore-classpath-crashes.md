# Keycloak JUnit NamespaceAwareStore classpath crashes

Status: fixed (classpath normalized; see Fix below). A separate, unrelated
missing-dependency issue was uncovered once this bug stopped masking it — see
Follow-on issue.

Date observed: 2026-07-02
Date fixed: 2026-07-02

## Summary

The expanded Keycloak `tests` and `testsuite` CratonVM run recorded 338
`CRASH` rows where JUnit 5 extension setup fails with:

```text
NoSuchMethodError
method="org/junit/jupiter/engine/execution/NamespaceAwareStore.computeIfAbsent(Ljava/lang/Object;Ljava/util/function/Function;)Ljava/lang/Object;"
caller="org/keycloak/testframework/KeycloakIntegrationTestExtension.getLogHandler(Lorg/junit/jupiter/api/extension/ExtensionContext;)Lorg/keycloak/testframework/LogHandler; @pc=28"
```

The failures are mostly in the new Keycloak JUnit 5 test framework:

```text
tests/base: 337
tests/clustering: 1
```

## Representative Rows

```text
module=tests/base
class=org.keycloak.tests.account.AccountConsoleDisabledTest
status=CRASH
rc=1
seconds=14.581
```

```text
module=tests/base
class=org.keycloak.tests.actions.RequiredActionUpdateProfileTest
status=CRASH
rc=1
seconds=14.360
```

## Initial Diagnosis

This signature is strongly tied to a mixed JUnit runtime classpath, not to 338
separate test defects.

`apps\keycloak\kc-universal-cp.txt` contains both old and new JUnit artifacts,
with the older jars appearing first:

```text
org/junit/jupiter/junit-jupiter-api/5.10.3
org/junit/jupiter/junit-jupiter-api/6.0.3
org/junit/jupiter/junit-jupiter-engine/5.10.3
org/junit/jupiter/junit-jupiter-engine/6.0.3
org/junit/platform/junit-platform-commons/1.10.3
org/junit/platform/junit-platform-commons/6.0.3
org/junit/platform/junit-platform-engine/1.10.3
org/junit/platform/junit-platform-engine/6.0.3
```

`javap` confirms the mismatch:

- `junit-jupiter-engine-5.10.3.jar` does not have
  `NamespaceAwareStore.computeIfAbsent(...)`.
- `junit-jupiter-engine-6.0.3.jar` does have
  `NamespaceAwareStore.computeIfAbsent(...)`.

A HotSpot probe with the same runner/classpath also fails this representative
class with a JUnit `NoSuchMethodError`, so this case is currently classified as
a runner/classpath bug. CratonVM reports it as process `CRASH` because the
linkage error terminates the VM before the JUnit summary line is emitted.

## Repro

The original run used:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File C:\craton\CratonVM\apps\keycloak-suite-runner\run-keycloak-suite.ps1 `
  -ClassList C:\craton\CratonVM-keycloak-runner-others-20260702-01\apps\keycloak-suite-runner\.suite\non238-tests-testsuite-20260702.tsv `
  -Category others -Vm craton -Jit on -Start 1 -Count 0 -Parallel 2 -TimeoutSec 600 `
  -RunName craton-others-20260702-01 `
  -KeycloakRoot C:\craton\CratonVM\apps\keycloak `
  -WorkDir C:\craton\CratonVM-keycloak-runner-others-20260702-01\apps\keycloak-suite-runner\.suite `
  -Exe C:\craton\CratonVM\target\release\cratonvm-keycloak-others-20260702-01.exe
```

HotSpot probe result:

```text
run=hotspot-crash-signature-probe-20260702-01
class=org.keycloak.tests.account.AccountConsoleDisabledTest
status=FAIL
note=java.lang.NoSuchMethodError: ExtensionContext$Store.computeIfAbsent(...)
```

## Evidence

```text
C:\craton\CratonVM-keycloak-runner-others-20260702-01\apps\keycloak-suite-runner\.suite\results\craton-others-20260702-01\others-jit\results.tsv
C:\craton\CratonVM-keycloak-runner-others-20260702-01\apps\keycloak-suite-runner\.suite\results\craton-others-20260702-01\others-jit\logs\tests_base.org.keycloak.tests.account.AccountConsoleDisabledTest.err.log
C:\craton\CratonVM-keycloak-runner-others-20260702-01\apps\keycloak-suite-runner\.suite\results\hotspot-crash-signature-probe-20260702-01\hotspot-jit\logs\tests_base.org.keycloak.tests.account.AccountConsoleDisabledTest.out.log
```

## Fix

`apps\keycloak\kc-universal-cp.txt` is a local, gitignored, machine-generated
file (not checked in, no in-repo generator script), so the fix was applied
directly to that file on this machine:

- Removed every JUnit 5.10.3 / junit-platform 1.10.3 jar
  (`junit-jupiter-api`, `junit-jupiter-engine`, `junit-jupiter-params`,
  `junit-platform-commons`, `junit-platform-engine`,
  `junit-platform-launcher`, `junit-vintage-engine`).
- Pinned every one of those artifacts to the single 6.0.3 release already
  present in `~/.m2/repository`, adding `junit-platform-launcher-6.0.3.jar`
  and `junit-vintage-engine-6.0.3.jar` (previously only 1.10.3/5.10.3 copies
  were on the classpath).
- Left `junit-4.13.2.jar` (plain JUnit 4, used via the vintage engine) alone.

Verified with a direct repro (`KcRunner org.keycloak.tests.account.
AccountConsoleDisabledTest` on `target\release\cratonvm.exe`): the
`NamespaceAwareStore.computeIfAbsent` `NoSuchMethodError` no longer occurs.

Rerun the 338 affected classes with the normalized classpath to confirm the
fix holds across the full set. Since `kc-universal-cp.txt` isn't tracked in
git, this fix needs to be re-applied (or, better, encoded into whatever
process regenerates the file) on any other machine/worktree that runs the
suite.

## Follow-on issue (separate bug, uncovered by this fix)

With the JUnit crash resolved, the same repro class now fails deeper in
Keycloak's new test framework config bootstrap:

```text
NoSuchMethodError: io/smallrye/config/SmallRyeConfigBuilder.addDefaultSources()...
  (fixed by adding smallrye-config/smallrye-config-common/smallrye-config-core
   3.16.0 to kc-universal-cp.txt — those jars were missing entirely)
```

then, after adding those:

```text
linkage error: no class def found: org/keycloak/testframework/config/Config
```

`javap` on `Config.class` shows `initConfig()` constructs
`io/quarkus/runtime/configuration/CharsetConverter`,
`MemorySizeConverter`, and `InetSocketAddressConverter` — all from
`quarkus-core`, which is entirely absent from `kc-universal-cp.txt` (only
`resteasy-reactive-common{,-types}` quarkus jars are present). This is a
distinct, pre-existing missing-dependency gap (quarkus-core and its
transitive deps were never added to the universal classpath), not a JUnit
version-mismatch issue. Needs its own investigation/fix — track separately.
