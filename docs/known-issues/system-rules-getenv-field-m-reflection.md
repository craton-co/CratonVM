# System Rules library's getenv() field-layout reflection mismatch

Status: open — narrow, single test affected

Date observed: 2026-07-04

## Summary

`testsuite/integration-arquillian/tests/base ::
org.keycloak.testsuite.authz.AuthzClientTest#testCreateWithEnvVars` fails
with:

```
java.lang.RuntimeException: System Rules expects System.getenv() to have a field 'm' but it has not
```

thrown from `Class.getDeclaredField`. The `org.junit.contrib.java.lang.system`
"System Rules" test library fakes environment variables for a test by
reflectively reaching into the internal field layout of the `Map` returned by
`System.getenv()` — it hardcodes the private field name `m`, matching real
OpenJDK's `ProcessEnvironment$StringEnvironment` internals. CratonVM's
`System.getenv()` backing object doesn't expose a field with that name (its
internal representation differs from real OpenJDK's), so the library's
reflection-based mocking trick fails outright.

## Scope

This is a real CratonVM-vs-HotSpot internal-layout difference, but it's only
reachable by code that deliberately reflects into JDK-private internals via
this specific library trick — narrow blast radius (1 class in this sweep).
Not a general `System.getenv()` correctness bug: normal (non-reflective)
`getenv()` calls are unaffected.

## Next steps

Two possible fixes, in increasing order of general applicability:
1. Give CratonVM's `System.getenv()` backing map a field literally named `m`
   holding the real backing data, matching OpenJDK's private layout closely
   enough for this one library's reflection to succeed (narrow, low-risk,
   mirrors OpenJDK exactly for this one case).
2. Skip if this is the only known consumer of this reflection trick in the
   suites tracked so far — not worth broad layout changes for one test.

## Evidence

`/data/wt-keycloak-full-20260704/apps/keycloak-suite-runner/.suite/results/kcfull-others-1124-20260704-v2/others-jit/logs/testsuite_integration-arquillian_tests_base.org.keycloak.testsuite.authz.AuthzClientTest.out.log`
