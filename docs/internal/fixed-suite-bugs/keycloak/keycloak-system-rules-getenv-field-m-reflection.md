# System Rules library's getenv() field-layout reflection mismatch

Status: fixed

Date observed: 2026-07-04

Date fixed: 2026-07-04

## Summary

`testsuite/integration-arquillian/tests/base ::
org.keycloak.testsuite.authz.AuthzClientTest#testCreateWithEnvVars` failed
with:

```
java.lang.RuntimeException: System Rules expects System.getenv() to have a field 'm' but it has not
```

thrown from `Class.getDeclaredField`. The `org.junit.contrib.java.lang.system`
System Rules test library fakes environment variables by reflectively reaching
into the internal field layout of the `Map` returned by `System.getenv()` and
hard-coding the private field name `m`, matching the OpenJDK unmodifiable-map
wrapper shape. CratonVM exposed the backing `HashMap` directly, so the
reflection-based mocking setup failed.

## Fix

- `System.getenv()` now returns CratonVM's unmodifiable-map wrapper around the
  real/synthetic `HashMap` backing instead of exposing the backing map directly.
- Reflection now exposes a private final `m` field on the
  `Collections$UnmodifiableMap` display shape and the internal wrapper stamp,
  with slot 0 pointing at the backing map.
- The default native registry now includes the public
  `Class.getDeclaredField(String)`, `Field.get(Object)`, and
  `setAccessible(boolean)` surfaces needed by System Rules' reflection path.
- `Field.get` now honors the inherited `AccessibleObject.override` flag as a
  fallback, matching the existing `Method` behavior when Java bytecode handles
  `setAccessible(true)`.

## Validation

- `cargo test -p cratonvm-vm --test wp8_10_10_system_getenv_map_class -- --nocapture`

## Scope

This was a CratonVM-vs-HotSpot private-layout difference, reachable by code
that deliberately reflects into JDK-private internals via this specific System
Rules trick. Normal non-reflective `System.getenv()` calls were unaffected.

## Follow-up

A future Keycloak rerun should verify that
`AuthzClientTest#testCreateWithEnvVars` advances past System Rules'
`System.getenv().getClass().getDeclaredField("m")` setup.

## Original evidence

`/data/wt-keycloak-full-20260704/apps/keycloak-suite-runner/.suite/results/kcfull-others-1124-20260704-v2/others-jit/logs/testsuite_integration-arquillian_tests_base.org.keycloak.testsuite.authz.AuthzClientTest.out.log`
