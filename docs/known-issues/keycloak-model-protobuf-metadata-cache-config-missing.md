# RealmModelTest fails after STW fix: protobuf metadata cache config missing

Status: open

Date observed: 2026-07-08, while verifying the fixed
[`keycloak-model-stw-takeover-hang-eventloopgroup-shutdown`](../internal/fixed-suite-bugs/keycloak-model-stw-takeover-hang-eventloopgroup-shutdown-FIXED.md)
residual with the real Keycloak suite runner.

## Summary

`RealmModelTest` no longer hangs during Netty `EventLoopGroup` shutdown under
CratonVM `--nojit`. The same real runner now exits cleanly in about 108s, but
fails class initialization with:

```text
java.lang.ExceptionInInitializerError
Caused by: org.infinispan.commons.CacheConfigurationException:
ISPN000436: Cache '___protobuf_metadata' has been requested, but no matching cache configuration exists
```

The failure is CratonVM-specific. The same class and classpath pass on HotSpot
with `-Xint`.

## Evidence

CratonVM `--nojit`:

```powershell
$suite = 'C:\craton\CratonVM\apps\keycloak-suite-runner\.suite'
$list = Join-Path $suite 'keycloak-model-realm-stw-20260708-001.tsv'
"module`tclass" | Set-Content -Path $list -Encoding ascii
"testsuite/model`torg.keycloak.testsuite.model.RealmModelTest" | Add-Content -Path $list -Encoding ascii

& 'C:\craton\CratonVM\apps\keycloak-suite-runner\run-keycloak-suite.ps1' `
  -ClassList $list -Category others -Vm craton -Jit off -Parallel 1 `
  -TimeoutSec 900 -RunName 'keycloak-stw-eventloopgroup-verify-20260708-001' `
  -KeycloakRoot 'C:\craton\CratonVM\apps\keycloak' -WorkDir $suite `
  -Exe 'C:\craton\cargo-targets\keycloak-stw-eventloopgroup-20260708-001\release\cratonvm-keycloak-stw-eventloopgroup-20260708-001.exe'
```

Result:

```text
status=FAIL seconds=107.958 containersFailed=1
KCRUNNER_RESULT tests=0 failed=0 aborted=0 skipped=0 containersFailed=1
```

Key stack:

```text
org.infinispan.manager.DefaultCacheManager.internalStart(DefaultCacheManager.java:750)
org.keycloak.connections.infinispan.DefaultInfinispanConnectionProviderFactory.getDefaultCacheManager(DefaultInfinispanConnectionProviderFactory.java:280)
org.keycloak.testsuite.model.KeycloakModelTest.<clinit>(KeycloakModelTest.java:306)
Caused by: org.infinispan.commons.CacheConfigurationException:
ISPN000436: Cache '___protobuf_metadata' has been requested, but no matching cache configuration exists
org.infinispan.configuration.ConfigurationManager.getConfiguration(ConfigurationManager.java:68)
org.infinispan.manager.DefaultCacheManager.wireAndStartCache(DefaultCacheManager.java:607)
org.infinispan.registry.impl.InternalCacheRegistryImpl.startInternalCaches(InternalCacheRegistryImpl.java:134)
org.infinispan.globalstate.impl.GlobalConfigurationManagerImpl.postStart(GlobalConfigurationManagerImpl.java:107)
```

HotSpot control:

```powershell
& 'C:\craton\CratonVM\apps\keycloak-suite-runner\run-keycloak-suite.ps1' `
  -ClassList $list -Category others -Vm hotspot -Jit off -Parallel 1 `
  -TimeoutSec 900 -RunName 'keycloak-stw-eventloopgroup-hotspot-control-20260708-001' `
  -KeycloakRoot 'C:\craton\CratonVM\apps\keycloak' -WorkDir $suite
```

Result:

```text
status=PASS seconds=142.6
```

## Initial hypothesis

This is likely in the same broad Infinispan real/synthetic configuration
surface as the earlier `DefaultCacheManager` fixes, but it is a distinct
failure. The internal cache registry asks for `___protobuf_metadata`, and
CratonVM's run has lost or failed to materialize the corresponding cache
configuration by the time `GlobalConfigurationManagerImpl.postStart()` starts
internal caches.

The STW selector fix is still confirmed: the CratonVM log reaches
`io.netty.channel.EventLoopGroup` `STOPPING` -> `STOPPED`, exits the runner,
and has no repeated STW takeover wait signature.
