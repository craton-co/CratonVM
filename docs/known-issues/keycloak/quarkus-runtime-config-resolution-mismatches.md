# quarkus/runtime: SmallRye/Quarkus Config resolution mismatches — wrong values, missing property names, host-environment leakage

Status: open — likely one underlying SmallRye Config resolution/layering gap manifesting as several distinct
symptoms across `quarkus/runtime`'s configuration test suite

Date observed: 2026-07-11 (refresh rerun against non-passed-before classes, branch fix/keycloak-nonpassed-rerun-v2-20260710)

## Summary

Several `quarkus/runtime :: configuration.*` test classes fail with config-value or config-property-enumeration
mismatches:

1. **`DatasourcesConfigurationTest::propagatedPropertyNames`** — expects the enumerated set of config property
   names to contain `"quarkus.datasource.jdbc.min-size"`, but the actual enumerated set instead contains a huge
   dump of unrelated **host machine environment variables and system properties** (`PATH`,
   `CLAUDE_CODE_SESSION_ID`, `ANTHROPIC_BASE_URL`, `CUDA_PATH_V12_6`, `chocolateylastpathupdate`, hundreds more) —
   none of which are Quarkus/Keycloak config values, and the specific expected Quarkus-generated property name is
   simply absent from the (very large) actual set.

2. **`TracingConfigurationTest::syslogLogMdcOn`** (and presumably others in the same class) — expects
   `quarkus.otel.enabled` to resolve to `"true"` but gets `"false"`.

3. **`IgnoredArtifactsTest`** — `AssertionError: Ignored artifacts does not comply with the specified artifacts for 'dev-file' JDBC driver`.

4. **`ConfigurationTest`** — `AssertionError: expected:<secret> but was:<null>` (a keystore-backed config value
   not resolving).

## Notes

- Item 1 is the most concrete and interesting: the test isn't failing because of an *incorrect* value for the
  expected property — it's failing because the **property-name enumeration itself returns something wildly
  different from what's expected**, seemingly the raw OS environment/system-properties space rather than a
  properly-scoped Quarkus/SmallRye config-source view. This looks like it could be a CratonVM-level gap in
  whatever `ConfigSource`/environment-variable-backed config source enumeration Quarkus's config test harness
  uses, where CratonVM's implementation of environment/property enumeration doesn't filter/map the same way real
  HotSpot's underlying `System.getenv()`/`System.getProperties()` (or whatever the ConfigSource wraps) would.
- Items 2-4 could be downstream consequences of the same layering/precedence gap (if the wrong config source or
  wrong precedence order is active, boolean/string defaults could resolve to unexpected values) or could be
  separate, narrower issues each needing individual attention — not yet distinguished.
- These are almost certainly **not** related to the already-known `remote-providers` Maven-resolution gap (that
  one is specific to `org.keycloak.it.utils.Maven`'s reactor dependency graph, not `quarkus/runtime`'s config
  test suite) — flagging as its own cluster.

## Next steps

1. For item 1 specifically: find what `ConfigSource` implementation is behind the "propagated property names"
   enumeration in `DatasourcesConfigurationTest` and compare its actual behavior (a raw environment dump) against
   what Quarkus/SmallRye Config expects (a properly-scoped, `quarkus.*`/`kc.*`-namespaced view) — check
   CratonVM's `System.getenv()` or equivalent environment-source implementation for anything that might expose
   the wrong scope/set of variables to this specific enumeration API.
2. For items 2-4: trace each individually, but consider re-checking after item 1 is fixed in case the property
   enumeration gap masks/distorts the config-value resolution for these tests too.
3. Given only a handful of classes are affected, this may be a narrower gap than it first appears — worth
   comparing against the many *other* `quarkus/runtime` config tests that pass cleanly (33 tests total in
   `DatasourcesConfigurationTest` alone, only 1 fails) to isolate what's specific to the failing cases.

## Repro

```
cd C:\craton\CratonVM-keycloak-nonpassed-v2-20260710
$jdk = '"C:\Program Files\Java\jdk-25"'
powershell -NoProfile -ExecutionPolicy Bypass -File apps\keycloak-suite-runner\run-keycloak-suite.ps1 -Vm craton -Jit on -TimeoutSec 60 -Parallel 1 -RunName repro-quarkus-config -ClassList <(printf 'module\tclass\nquarkus/runtime\torg.keycloak.quarkus.runtime.configuration.DatasourcesConfigurationTest\n') -KeycloakRoot apps\keycloak -Exe target\release\cratonvm-nonpassed-v2-refresh-20260711.exe -JdkHome $jdk
```

## Evidence

`C:\craton\CratonVM-keycloak-nonpassed-v2-20260710\apps\keycloak-suite-runner\.suite\results\nonpassed-before-refresh-shard1\all-jit\logs\quarkus_runtime.org.keycloak.quarkus.runtime.configuration.{DatasourcesConfigurationTest,TracingConfigurationTest,IgnoredArtifactsTest,ConfigurationTest}.out.log`,
2026-07-11 refresh rerun with a binary built from current `dev`.
