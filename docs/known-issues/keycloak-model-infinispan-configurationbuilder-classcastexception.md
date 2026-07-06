# Infinispan ConfigurationBuilder.build() ClassCastException

Status: open

Date observed: 2026-07-06

## Summary

Follow-up to
[keycloak-model-infinispan-globalconfiguration-isclustered-nosuchmethod-FIXED](../internal/fixed-suite-bugs/keycloak-model-infinispan-globalconfiguration-isclustered-nosuchmethod-FIXED.md):
fixing `GlobalConfigurationBuilder.build()`'s identity-wrapper native (letting
real bytecode construct a genuine `GlobalConfiguration`) let
`CoreConfigurationSerializer.writeCacheContainer` reach one step further in
the same boot path, where it does
`(Configuration) configurationBuilder.build()` and now throws:

```text
java.lang.ClassCastException: org.infinispan.configuration.cache.ConfigurationBuilder
cannot be cast to org.infinispan.configuration.cache.Configuration
```

This was unreachable before the `isClustered()` fix (masked by the earlier
VM-level crash) — it is a **second instance of the exact same bug shape**:
`org.infinispan.configuration.cache.ConfigurationBuilder.build()` is natively
shimmed as a pure identity wrapper (`native_cfg_build` in
`native-builtins/src/infinispan_local.rs`) that returns `this` (the Builder)
relabeled as the return type, instead of constructing a genuinely distinct
`Configuration` object. Any caller that casts the "Configuration" back to its
real type — `writeCacheContainer` does exactly that — gets a real
`ClassCastException` because the receiver's actual runtime class genuinely is
`ConfigurationBuilder`.

## Why this one is harder to fix than `GlobalConfigurationBuilder.build()`

Unlike the `GlobalConfiguration` fix, `ConfigurationBuilder`/`Configuration`
can't simply have its native `build()` override removed: a separate native,
`native_dcm_define_configuration` (`DefaultCacheManager.defineConfiguration`),
reads `CONFIG_FIELD_SIZE_LIMIT` / `CONFIG_FIELD_TTL_MS` off the `Configuration`
argument by **raw synthetic-layout slot index**. If real
`ConfigurationBuilder.build()` bytecode ran instead, it would return a real
`Configuration` with the genuine (much larger, differently-ordered) field
layout, and those raw-index reads would silently read the wrong slot (or
panic).

## Next steps

- Rework `native_dcm_define_configuration` (and any other native in
  `infinispan_local.rs` that reads `Configuration`'s fields by raw slot
  index) to read the real `Configuration` object's fields by name/accessor
  method instead of by synthetic layout position — mirroring how the
  `GlobalConfiguration` fix confirmed `native_dcm_init` already ignores its
  `GlobalConfiguration` argument entirely (no rework needed there).
- Once no native reads `Configuration` by raw slot index, remove
  `native_cfg_build` and its registration on
  `ConfigurationBuilder.build()`, the same way
  `native_global_cfg_build` was removed for `GlobalConfigurationBuilder`.
- Re-run the `testsuite/model` `RealmModelTest` repro (and the full 37-class
  batch) once fixed; expect real PASS/FAIL/SKIP outcomes for cache
  configuration paths that were previously masked by this ClassCastException.

## Repro

```powershell
$list = "C:\temp\keycloak-model-one.tsv"
"module`tclass" | Set-Content -Path $list -Encoding ascii
"testsuite/model`torg.keycloak.testsuite.model.RealmModelTest" | Add-Content -Path $list -Encoding ascii

powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File "apps\keycloak-suite-runner\run-keycloak-suite.ps1" `
  -ClassList $list -Category others -Vm craton -Jit on -Parallel 1 -TimeoutSec 300 `
  -RunName kcmodel-configurationbuilder-cce-repro `
  -KeycloakRoot "<keycloak-checkout>" `
  -WorkDir "apps\keycloak-suite-runner\.suite" `
  -Exe "target\release\cratonvm.exe"
```

Expected current failure: `ClassCastException:
org.infinispan.configuration.cache.ConfigurationBuilder cannot be cast to
org.infinispan.configuration.cache.Configuration` during
`CoreConfigurationSerializer.writeCacheContainer`, reached from the same
`testsuite/model` cache-bootstrap path as the now-fixed `isClustered()` bug.
