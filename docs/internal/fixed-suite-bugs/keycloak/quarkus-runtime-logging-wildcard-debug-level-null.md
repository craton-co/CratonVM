# LoggingConfigurationTest wildcard DEBUG level resolves null

Status: fixed

Date observed: 2026-07-04
Date fixed: 2026-07-05

## Summary

`LoggingConfigurationTest#testWildcardOptionFromConfigFile` expected the
wildcard-configured log category to resolve to `DEBUG`, but CratonVM returned
`null`.

## Root Cause

This was the same reset-state defect as the sibling
`quarkus-runtime-logging-getpropertynames-garbage-key.md` finding. CratonVM did
not replace the global System properties object/table when Keycloak reset test
configuration with `System.setProperties(...)`, so stale config arguments could
survive into the next SmallRye config build and disturb wildcard resolution.

## Fix

`System.setProperties(Properties)` now replaces the cached singleton and the
VM-wide system-property table. The `Properties` side table is also updated so
mutations through the new singleton are reflected by `System.getProperty`.

## Validation

- Linux remote no-JIT: `LoggingConfigurationTest` passed all 29 tests.
- Windows JDK 25 no-JIT: `LoggingConfigurationTest` passed all 29 tests in
  `kc-quarkus-config-three-winprobe-20260705-002`.
