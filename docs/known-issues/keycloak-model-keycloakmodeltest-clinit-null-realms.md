# Keycloak model KeycloakModelTest static initializer null realms

Status: open

Date observed: 2026-07-03

## Summary

After updating from `dev` and rerunning only the previously non-passed
Keycloak classes with the per-module classpath runner, 37 `testsuite/model`
classes still crash under CratonVM before any test method executes.

The old `Assert.assertNotNull` and missing `quarkus-core` classpath failures
are no longer the active signature for this bucket. The current failure reaches
`org.keycloak.testsuite.model.KeycloakModelTest` static initialization and then
wraps a `NullPointerException` in `ExceptionInInitializerError`:

```text
<clinit> failed - wrapping in ExceptionInInitializerError
class=org/keycloak/testsuite/model/KeycloakModelTest
cause=java/lang/NullPointerException Cannot invoke
"org.keycloak.models.RealmProvider.getRealmsWithProviderTypeStream(java.lang.Class)"
because the return value of "org.keycloak.models.KeycloakSession.realms()" is null
```

CratonVM reports the top-level process error as:

```text
[cratonvm] main-vm run() returned Err: Error in thread "main" linkage error:
no class def found: org/keycloak/testsuite/model/KeycloakModelTest
```

## Evidence

Run:

```text
craton-nonpassed-dev-20260703-01 / others-jit
```

Result file:

```text
C:\craton\CratonVM-keycloak-nonpassed-rerun-20260703-01\apps\keycloak-suite-runner\.suite\results\craton-nonpassed-dev-20260703-01\others-jit\results.tsv
```

Representative stderr log:

```text
C:\craton\CratonVM-keycloak-nonpassed-rerun-20260703-01\apps\keycloak-suite-runner\.suite\results\craton-nonpassed-dev-20260703-01\others-jit\logs\testsuite_model.org.keycloak.testsuite.model.authz.ConcurrentAuthzTest.err.log
```

Affected classes:

```text
org.keycloak.testsuite.model.authz.ConcurrentAuthzTest
org.keycloak.testsuite.model.client.ClientModelTest
org.keycloak.testsuite.model.clientscope.ClientScopeModelTest
org.keycloak.testsuite.model.clientscope.ClientScopeStorageTest
org.keycloak.testsuite.model.DBLockTest
org.keycloak.testsuite.model.events.AdminEventQueryTest
org.keycloak.testsuite.model.events.EventQueryTest
org.keycloak.testsuite.model.exportimport.ExportModelTest
org.keycloak.testsuite.model.exportimport.ImportModelTest
org.keycloak.testsuite.model.FederatedIdentityModelTest
org.keycloak.testsuite.model.group.GroupModelTest
org.keycloak.testsuite.model.infinispan.CacheExpirationTest
org.keycloak.testsuite.model.infinispan.EmbeddedInfinispanSplitBrainTest
org.keycloak.testsuite.model.infinispan.FeatureEnabledTest
org.keycloak.testsuite.model.infinispan.InfinispanIckleQueryTest
org.keycloak.testsuite.model.infinispan.RetryAndBackOffTest
org.keycloak.testsuite.model.loginfailure.RemoteLoginFailureTest
org.keycloak.testsuite.model.MigrationModelTest
org.keycloak.testsuite.model.MultiSiteProfileTest
org.keycloak.testsuite.model.RealmModelTest
org.keycloak.testsuite.model.role.RoleModelTest
org.keycloak.testsuite.model.session.AuthenticationSessionTest
org.keycloak.testsuite.model.session.OfflineSessionPersistenceTest
org.keycloak.testsuite.model.session.SessionTimeoutsTest
org.keycloak.testsuite.model.session.UserSessionConcurrencyTest
org.keycloak.testsuite.model.session.UserSessionExpirationTest
org.keycloak.testsuite.model.session.UserSessionInitializerTest
org.keycloak.testsuite.model.session.UserSessionPersisterProviderTest
org.keycloak.testsuite.model.session.UserSessionProviderModelTest
org.keycloak.testsuite.model.session.UserSessionProviderOfflineModelTest
org.keycloak.testsuite.model.singleUseObject.SingleUseObjectModelTest
org.keycloak.testsuite.model.TimeOffsetTest
org.keycloak.testsuite.model.transaction.StorageTransactionTest
org.keycloak.testsuite.model.user.FederatedUserTest
org.keycloak.testsuite.model.user.UserModelTest
org.keycloak.testsuite.model.user.UserPaginationTest
org.keycloak.testsuite.model.user.UserSyncTest
```

## Repro

Use the module classpath runner against one affected class:

```powershell
$list = "C:\temp\keycloak-model-one.tsv"
"module`tclass" | Set-Content -Path $list -Encoding ascii
"testsuite/model`torg.keycloak.testsuite.model.authz.ConcurrentAuthzTest" | Add-Content -Path $list -Encoding ascii

powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File "C:\craton\CratonVM\apps\keycloak-suite-runner\run-keycloak-suite.ps1" `
  -ClassList $list `
  -Category others `
  -Vm craton `
  -Jit on `
  -Parallel 1 `
  -TimeoutSec 600 `
  -RunName keycloak-model-null-realms-repro `
  -KeycloakRoot "C:\craton\CratonVM\apps\keycloak" `
  -WorkDir "C:\craton\CratonVM\apps\keycloak-suite-runner\.suite" `
  -Exe "C:\craton\CratonVM\target\release\cratonvm.exe"
```

## Current assessment

This is no longer the earlier universal-classpath missing-jar problem. The
module classpath contains enough of the Keycloak/Quarkus stack to enter
`KeycloakModelTest`, then a static initialization path observes a null
`KeycloakSession.realms()` provider.

The failing path may be a CratonVM behavioral bug in static initialization,
service/bootstrap ordering, provider discovery, or a native/reflective call
that returns a partially initialized `KeycloakSession`. A HotSpot comparison
using the same generated module classpath is still required before narrowing
the owner further.

## Next steps

- Re-run one affected class under HotSpot with the exact same runner-generated
  `testsuite/model` classpath.
- If HotSpot passes, instrument `KeycloakModelTest` static initialization and
  `KeycloakSession.realms()` provider setup under CratonVM.
- Check whether any synthetic-stub fallback appears before the null provider;
  absence of a terminal `NoSuchMethodError` does not rule out earlier masked
  classpath/provider initialization damage.
