# Keycloak test-framework Quarkus config classpath gap

Status: open

Date observed: 2026-07-02

## Summary

Uncovered while fixing
[keycloak-junit-namespaceawarestore-classpath-crashes](../internal/keycloak-junit-namespaceawarestore-classpath-crashes.md)
(now fixed and moved to `docs/internal/`). Once the mixed-JUnit-version
crash stopped masking everything downstream, the same repro class fails
deeper in Keycloak's new JUnit 5 test framework's config bootstrap.

`org.keycloak.testframework.config.Config.initConfig()` needs
`io.smallrye.config.SmallRyeConfigBuilder` (added — see Fix below) and then
constructs `io/quarkus/runtime/configuration/CharsetConverter`,
`MemorySizeConverter`, and `InetSocketAddressConverter`, all from
`quarkus-core`. `quarkus-core` and its transitive dependencies are entirely
absent from `apps\keycloak\kc-universal-cp.txt` — only
`resteasy-reactive-common` / `resteasy-reactive-common-types` quarkus jars
are present.

## Repro

```powershell
$KC  = "C:/craton/CratonVM/apps/keycloak"
$CV  = "C:/craton/CratonVM/target/release/cratonvm.exe"
$JDK = "C:/Program Files/Java/jdk-25"
$CP  = "$KC/kc-runner;" + (Get-Content "$KC/kc-universal-cp.txt" -Raw).Trim()
& $CV --java-home $JDK --stack-dump-on-timeout 0 -cp $CP KcRunner org.keycloak.tests.account.AccountConsoleDisabledTest
```

Current failure:

```text
linkage error: no class def found: org/keycloak/testframework/config/Config
```

(`Config`'s static initializer throws `NoClassDefFoundError` while resolving
`io/quarkus/runtime/configuration/*`, which are not on the classpath.)

## Partial fix already applied

`smallrye-config`, `smallrye-config-common`, `smallrye-config-core` (version
3.16.0, matching the version already used by
`apps\keycloak\quarkus\config-api\cratonvm-full-cp.txt`) were added to
`kc-universal-cp.txt` — those were missing outright, not just
version-mismatched. That resolved the `SmallRyeConfigBuilder.
addDefaultSources()` `NoSuchMethodError` but exposed the `quarkus-core` gap
above.

## Next Steps

- Identify the full `quarkus-core` (and transitive) jar set needed by
  `org.keycloak.testframework.config.Config` and add it to
  `kc-universal-cp.txt`, matching versions already used elsewhere in the repo
  (e.g. `quarkus/config-api/cratonvm-full-cp.txt`) where possible.
- Since `kc-universal-cp.txt` is local/gitignored with no in-repo generator
  script, consider adding one so this classpath can be reproduced
  deterministically instead of hand-patched.
- Rerun the previously-338-crash class list once the classpath is complete.
