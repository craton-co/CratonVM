# Infinispan ConfigurationBuilder.build() ClassCastException

Status: fixed on 2026-07-06 — the `ClassCastException` is gone from `testsuite/model` cache bootstrap; the same class now reaches a distinct, unrelated Netty reflection residual (see below).

## Fix

Same root cause and same fix shape as the sibling
[GlobalConfigurationBuilder.isClustered() fix](keycloak-model-infinispan-globalconfiguration-isclustered-nosuchmethod-FIXED.md):
`org.infinispan.configuration.cache.ConfigurationBuilder.build()` was
natively shimmed (`native_cfg_build` in `../../../../native-builtins/src/infinispan_local.rs`,
now removed) as a pure identity wrapper returning `this` (the Builder)
relabeled as the return type, instead of letting real bytecode construct a
genuinely distinct `Configuration`. Real Infinispan's
`CoreConfigurationSerializer.writeCacheContainer` casts the "Configuration"
back to its real type, so it threw a real `ClassCastException` naming
`ConfigurationBuilder` — 100% correct given the receiver's actual runtime
class genuinely was the Builder.

This one couldn't be fixed the same trivial way as `GlobalConfigurationBuilder`
though, because a second native — `native_dcm_define_configuration`
(`DefaultCacheManager.defineConfiguration`) — read `CONFIG_FIELD_SIZE_LIMIT`
/ `CONFIG_FIELD_TTL_MS` off the `Configuration` argument by **raw
synthetic-layout slot index**. Un-shimming `build()` alone would have let
real bytecode return a genuine `Configuration` with the real (much larger,
`~18`-field, differently-ordered) layout, and those raw-index reads would
have silently read garbage.

The complete fix, in two parts:

1. **`native_dcm_define_configuration` reworked** to read size/ttl through
   `Configuration`'s real accessor API via `invoke_virtual` instead of raw
   field-slot reads: `configuration.memory()` → `MemoryConfiguration`, then
   `.maxCount()` (`long`, real Infinispan API, confirmed via `javap` against
   `infinispan-core-16.0.8.jar`); `configuration.expiration()` →
   `ExpirationConfiguration`, then `.lifespan()` (`long` ms). Both fall back
   to the pre-existing defaults (`DEFAULT_SIZE_LIMIT`, no TTL) if the
   returned value is absent/non-positive, matching the original code's
   defensive style.
2. **`native_cfg_build` removed** and its registration on
   `ConfigurationBuilder.build()` dropped, letting real
   `ConfigurationBuilder.build()` bytecode run and construct a genuinely
   distinct, correctly-typed `Configuration` — mirroring exactly how
   `native_global_cfg_build` was removed for `GlobalConfigurationBuilder`.

The now-dead `CONFIG_FIELD_NAME`/`CONFIG_FIELD_SIZE_LIMIT`/
`CONFIG_FIELD_TTL_MS`/`CONFIG_NUM_FIELDS` constants were removed too (no
remaining readers in the file). `../../../../classloading/src/class_manager.rs`'s
`synthetic_stub_fields` entries for `Configuration`/`ConfigurationBuilder`
were deliberately left in place, unchanged — same as the precedent set by
the `GlobalConfiguration`/`GlobalConfigurationBuilder` fix: they're a
harmless field-count padding floor (real Infinispan's field count is far
larger, so `num_total_fields.max(stub_total)` is a no-op), not something
this fix needed to touch.

## Regression test

`native-builtins/src/infinispan_local.rs::tests::t19_10_define_configuration_reads_via_real_accessor_api_not_raw_slots`
— stands a real `Configuration` object in for with a mock whose
`invoke_virtual` hook only understands `memory()`/`maxCount()`/
`expiration()`/`lifespan()` (no synthetic slots at all), scripted to return
777/45000. Asserts the resulting cache's `size_limit`/`default_ttl` match
those scripted values (not the module's defaults) — if the native
regressed back to a raw `get_field(obj, N)` read, the mock's zeroed-default
heap would silently produce the wrong (default) values instead of failing
loudly, so the test explicitly asserts against the default too.

## Validation

- Repro (`RealmModelTest` via `run-keycloak-suite.ps1`): before this fix,
  `ClassCastException: org.infinispan.configuration.cache.ConfigurationBuilder
  cannot be cast to org.infinispan.configuration.cache.Configuration` during
  `CoreConfigurationSerializer.writeCacheContainer`. After: that exception
  is completely gone from the logs (grepped both stdout/stderr, zero hits
  for "ClassCastException"/"ConfigurationBuilder"/"isClustered") — the test
  now reaches a real, different, unrelated failure one layer deeper in
  Infinispan's embedded cache manager bootstrap (Netty's
  `PlatformDependent0` reflective `setAccessible` gate), tracked separately:
  [keycloak-model-netty-reflective-setaccessible-disabled.md](../../known-issues/keycloak-model-netty-reflective-setaccessible-disabled.md).
- `cargo test -p cratonvm-native-builtins infinispan`: 21/21 passed (20
  pre-existing + 1 new), no regressions.
- Built and used a dedicated `cratonvm-configbuildercce.exe` binary
  (branch `fix/kc-infinispan-configurationbuilder-cce-20260706`).

## Repro (kept for reference)

```powershell
$list = "C:\temp\kc-cce-verify.tsv"
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
