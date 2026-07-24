# TelemetryConfigurationTest service name leaked from prior test

Status: fixed

Date observed: 2026-07-04
Date fixed: 2026-07-05

## Summary

`TelemetryConfigurationTest#rootDefaults` expected
`telemetry-service-name=keycloak`, but CratonVM returned `something3`.

## Root Cause

`something3` is a real fixture value from a later telemetry test. It leaked
because CratonVM did not implement `System.setProperties(Properties)` as a true
replacement of the process System properties state. Keycloak's
`AbstractConfigurationTest.resetConfiguration()` therefore did not fully reset
CLI/config properties between tests.

## Fix

The native System/Properties bridge now replaces the VM-wide property table,
swaps the cached `System.getProperties()` singleton, and keeps the active
`Properties` side table synchronized across `setProperties`, `setProperty`, and
`clear`.

## Validation

- Linux remote no-JIT: `TelemetryConfigurationTest` passed all 6 tests.
- Windows JDK 25 no-JIT: `TelemetryConfigurationTest` passed all 6 tests in
  `kc-quarkus-config-three-winprobe-20260705-002`.
