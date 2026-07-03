# Keycloak System Rules expects System.getenv unmodifiable map field m

Status: open

Date observed: 2026-07-03

## Summary

One Keycloak legacy test fails because the System Rules library reflectively
depends on the private JDK implementation shape of `System.getenv()`.

Observed failure:

```text
java.lang.RuntimeException:
System Rules expects System.getenv() to have a field 'm' but it has not.
```

Affected class and method:

```text
org.keycloak.testsuite.authz.AuthzClientTest:testCreateWithEnvVars
```

The test itself is trying to temporarily mutate environment variables for the
duration of the test. System Rules does that by reflecting into the
`Collections$UnmodifiableMap` returned by HotSpot's `System.getenv()` and
reading its private backing-map field named `m`.

CratonVM's `System.getenv()` implementation now returns a cached process-wide
map singleton, but it does not expose the exact private
`java.util.Collections$UnmodifiableMap` layout expected by this library. That
breaks code that relies on HotSpot internals rather than the public Java API.

## Evidence

Run:

```text
craton-nonpassed-dev-20260703-01 / others-jit
```

Result file:

```text
C:\craton\CratonVM-keycloak-nonpassed-rerun-20260703-01\apps\keycloak-suite-runner\.suite\results\craton-nonpassed-dev-20260703-01\others-jit\results.tsv
```

Representative logs:

```text
C:\craton\CratonVM-keycloak-nonpassed-rerun-20260703-01\apps\keycloak-suite-runner\.suite\results\craton-nonpassed-dev-20260703-01\others-jit\logs\testsuite_integration-arquillian_tests_base.org.keycloak.testsuite.authz.AuthzClientTest.out.log
C:\craton\CratonVM-keycloak-nonpassed-rerun-20260703-01\apps\keycloak-suite-runner\.suite\results\craton-nonpassed-dev-20260703-01\others-jit\logs\testsuite_integration-arquillian_tests_base.org.keycloak.testsuite.authz.AuthzClientTest.err.log
```

JUnit summary:

```text
Failures (1):
  JUnit Vintage:AuthzClientTest:testCreateWithEnvVars
    => java.lang.RuntimeException:
       System Rules expects System.getenv() to have a field 'm' but it has not.
       java.lang.Class.getDeclaredField(Class.java:2382)
```

## Repro

Use the module classpath runner against the affected class:

```powershell
$list = "C:\temp\keycloak-systemrules-env-one.tsv"
"module`tclass" | Set-Content -Path $list -Encoding ascii
"testsuite/integration-arquillian/tests/base`torg.keycloak.testsuite.authz.AuthzClientTest" |
  Add-Content -Path $list -Encoding ascii

powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File "C:\craton\CratonVM\apps\keycloak-suite-runner\run-keycloak-suite.ps1" `
  -ClassList $list `
  -Category others `
  -Vm craton `
  -Jit on `
  -Parallel 1 `
  -TimeoutSec 600 `
  -RunName keycloak-systemrules-env-field-m-repro `
  -KeycloakRoot "C:\craton\CratonVM\apps\keycloak" `
  -WorkDir "C:\craton\CratonVM\apps\keycloak-suite-runner\.suite" `
  -Exe "C:\craton\CratonVM\target\release\cratonvm.exe"
```

Smaller probe:

```java
public class EnvShapeProbe {
  public static void main(String[] args) throws Exception {
    Object env = System.getenv();
    System.out.println(env.getClass().getName());
    System.out.println(env.getClass().getDeclaredField("m"));
  }
}
```

HotSpot exposes a `java.util.Collections$UnmodifiableMap` with field `m`.
CratonVM currently does not expose that field shape.

## Current assessment

This is a compatibility issue with libraries that reflect into JDK internals.
It is not a public `System.getenv()` API mismatch by itself; the earlier
singleton identity bug for `System.getenv()` / `System.getProperties()` is
already fixed. This bug is narrower: the returned map must either be a real
JDK `Collections$UnmodifiableMap` wrapper around a mutable backing map, or
CratonVM must otherwise support the reflective field access pattern expected by
System Rules.

## Next steps

- Decide whether CratonVM should exactly mirror HotSpot's
  `Collections.unmodifiableMap(backing)` object for `System.getenv()`.
- If yes, return the real unmodifiable wrapper and root both wrapper and
  backing map as process-wide singletons.
- Add a regression probe for `System.getenv().getClass().getDeclaredField("m")`
  and for mutating the backing map through System Rules' access pattern.
