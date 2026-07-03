# Keycloak model tests: Infinispan GlobalConfiguration.isClustered() NoSuchMethodError

Status: open

Date observed: 2026-07-03

## Summary

All 37 `testsuite/model` classes (see resolved doc
`docs/internal/keycloak-model-keycloakmodeltest-clinit-null-realms.md` for how
they got this far) now reach real Infinispan `GlobalConfiguration`
serialization during `KeycloakModelTest.<clinit>` and fail identically with:

```text
NoSuchMethodError method="org/infinispan/configuration/global/GlobalConfigurationBuilder.isClustered()Z"
caller="org/infinispan/configuration/serializing/CoreConfigurationSerializer.writeJGroups(Lorg/infinispan/commons/configuration/io/ConfigurationWriter;Lorg/infinispan/configuration/global/GlobalConfiguration;)V @pc=4"
```

which surfaces as the top-level:

```text
[cratonvm] main-vm run() returned Err: Error in thread "main" linkage error:
no class def found: org/keycloak/testsuite/model/KeycloakModelTest
```

## Evidence

`isClustered()` does **not** exist on `GlobalConfigurationBuilder`
(`infinispan-core-16.0.8.jar`, confirmed via `javap`) — only `clusteredDefault()`
/ `nonClusteredDefault()` / `defaultClusteredBuilder()`. It **does** exist on
`GlobalConfiguration` (also confirmed via `javap`: `public boolean
isClustered();`), which matches the caller's own descriptor
(`writeJGroups(..., GlobalConfiguration)` — the second parameter is
`GlobalConfiguration`, not `GlobalConfigurationBuilder`).

No duplicate/stale `infinispan-core` jar on the classpath (checked: only
`16.0.8` present, plus the `-tests` classifier jar which does not redeclare
`GlobalConfiguration`/`GlobalConfigurationBuilder`).

This looks like a CratonVM method-resolution or class-identity bug: the real
bytecode's `invokevirtual` should resolve against `GlobalConfiguration`
(which has the method), but CratonVM's `NoSuchMethodError` diagnostic
attributes the failed lookup to `GlobalConfigurationBuilder` (which
doesn't) — suggesting the interpreter is looking up the vtable/method table
for the wrong class, not that the method is genuinely missing from the
classpath.

## Repro

```powershell
$list = "C:\temp\keycloak-model-one.tsv"
"module`tclass" | Set-Content -Path $list -Encoding ascii
"testsuite/model`torg.keycloak.testsuite.model.RealmModelTest" | Add-Content -Path $list -Encoding ascii

powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File "C:\craton\CratonVM\apps\keycloak-suite-runner\run-keycloak-suite.ps1" `
  -ClassList $list `
  -Category others `
  -Vm craton `
  -Jit on `
  -Parallel 1 `
  -TimeoutSec 300 `
  -RunName kcmodel-infinispan-isclustered-repro `
  -KeycloakRoot "C:\craton\CratonVM\apps\keycloak" `
  -WorkDir "C:\craton\CratonVM\apps\keycloak-suite-runner\.suite" `
  -Exe "C:\craton\CratonVM\target\release\cratonvm.exe"
```

(Requires the harness fix from the resolved doc above to already be present —
without it, this repro fails earlier with the old `<clinit>` null-realms NPE
instead of reaching this point.)

## Next steps

- Instrument/trace CratonVM's method resolution for
  `GlobalConfiguration.isClustered()` — confirm whether the interpreter is
  looking up the method table keyed by the wrong `ClassId`
  (`GlobalConfigurationBuilder` instead of `GlobalConfiguration`), e.g. a
  stale/aliased class-identity cache, an incorrect `invokevirtual` receiver
  type resolution, or a JIT/interpreter fast-path confusing the two
  similarly-named, closely-related classes.
- A HotSpot comparison on the exact same classpath would further confirm
  this is CratonVM-specific (expected: HotSpot resolves `isClustered()`
  cleanly since real Infinispan config serialization is a well-trodden path
  in production Keycloak).
- Once fixed, re-run all 37 `testsuite/model` classes; expect a mix of real
  PASS/FAIL/SKIP outcomes (e.g. `ConcurrentAuthzTest` reached a legitimate
  Hibernate connection-pool-exhaustion test failure under concurrent load
  once this and the resolved issues above were fixed), not further VM-level
  crashes — that would confirm this is the last blocker for the batch.
