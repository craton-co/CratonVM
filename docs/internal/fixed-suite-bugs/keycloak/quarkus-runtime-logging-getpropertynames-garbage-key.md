# LoggingConfigurationTest getPropertyNames stale log-category key

Status: fixed

Date observed: 2026-07-04
Date fixed: 2026-07-05

## Summary

Two `LoggingConfigurationTest` sub-tests failed during config setup with:

```
logging category 'reproducer.not^ok' is not valid
```

The key was not garbage memory. It was a real fixture value from another test
that survived a test reset.

## Root Cause

`KeycloakMain.reset(SYSTEM_PROPERTIES)` calls
`System.setProperties((Properties) systemProperties.clone())`. CratonVM did not
implement `java.lang.System.setProperties(Properties)` as a replacement of the
VM-wide system properties table and the cached `System.getProperties()` object.
As a result, CLI/config properties from prior tests stayed visible during later
SmallRye `getPropertyNames()` walks.

## Fix

The native System/Properties bridge now:

- Implements `System.setProperties(Properties)` as a replacement, not an
  additive no-op.
- Replaces the cached System properties singleton.
- Clears stale VM system-property entries when the System properties object is
  cleared.
- Keeps the side table for the current `Properties` object synchronized.

## Validation

- Linux remote no-JIT: `LoggingConfigurationTest` passed all 29 tests.
- Windows JDK 25 no-JIT: `LoggingConfigurationTest` passed all 29 tests in
  `kc-quarkus-config-three-winprobe-20260705-002`.
