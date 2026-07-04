# Keycloak JUnit 5: Quarkus generated config mapping class not found

Status: open

Date observed: 2026-07-03

## Summary

After the PreviewFeatures native fix, 283 Keycloak JUnit 5 rows still crash on
runtime-generated Quarkus/SmallRye config mapping implementation classes.

Most rows fail on:

```text
[cratonvm] main-vm run() returned Err: Error in thread "main" linkage error: no class def found: io/quarkus/runtime/logging/LogBuildTimeConfig$$CMImpl
```

The two `tests/clustering` rows fail on the same shape for:

```text
io/quarkus/deployment/dev/testing/TestConfig$$CMImpl
```

## Evidence

Run:

```text
craton-azure-nonpassed-dev-20260703-previewfeatures-fixed-01
```

Counts:

| Signature | Count |
|---|---:|
| `io/quarkus/runtime/logging/LogBuildTimeConfig$$CMImpl` `no class def found` | 281 |
| `io/quarkus/deployment/dev/testing/TestConfig$$CMImpl` `no class def found` | 2 |

Representative logs:

```text
/home/victor/wt-keycloak-previewfeatures-suite-20260703-01/apps/keycloak-suite-runner/.suite/results/craton-azure-nonpassed-dev-20260703-previewfeatures-fixed-01/others-jit/logs/tests_base.org.keycloak.tests.account.AccountRestServiceRolesTest.err.log
/home/victor/wt-keycloak-previewfeatures-suite-20260703-01/apps/keycloak-suite-runner/.suite/results/craton-azure-nonpassed-dev-20260703-previewfeatures-fixed-01/others-jit/logs/tests_clustering.org.keycloak.tests.clustering.JdbcPingCustomSchemaTest.err.log
```

## Current conclusion

This is not the old `$$CMImpl.getSecrets()` `NoSuchMethodError` that was fixed
by upgrading synthetic stubs in place. The current failure is earlier and
coarser: by-name resolution still reports that the generated implementation
class itself is unavailable.

SmallRye config mapping implementations with the `$$CMImpl` suffix are runtime
generated, not jar-shipped classes. CratonVM should allow the probe/generate/
define path to behave like HotSpot and then resolve the newly defined class.
The current rerun shows that path is still incomplete for the Keycloak JUnit 5
test-framework modules.

## Next steps

- Compare one representative class on HotSpot with the same module classpath to
  confirm this is not a runner classpath gap.
- Trace `ClassLoader.loadClass`, `MethodHandles.Lookup.defineClass`, and
  subsequent by-name lookup for `LogBuildTimeConfig$$CMImpl`.
- Check whether the previous synthetic-stub upgrade fix covers only one loader
  slot or method lookup path, leaving this `no class def found` path unresolved.
