# crypto/fips1402: CryptoIntegration finds zero CryptoProvider services

Status: open

Date observed: 2026-07-04

## Summary

All 21 concrete test classes in `crypto/fips1402` fail identically at the
JUnit `@Rule`/container level (`CryptoInitRule.before`), before the test body
runs:

```
KCRUNNER_RESULT tests=0 failed=0 aborted=0 skipped=0 containersFailed=1
java.lang.IllegalStateException: Not able to load any cryptoProvider with the classLoader ...
    at org.keycloak.common.crypto.CryptoIntegration.detectProvider(...)
```

`CryptoIntegration.detectProvider` calls
`ServiceLoader.load(CryptoProvider.class, classLoader)` and gets zero
providers back. The `META-INF/services/org.keycloak.common.crypto.CryptoProvider`
descriptor file demonstrably exists on disk under
`crypto/fips1402/target/classes/` (and the sibling `crypto/default`,
`crypto/elytron` modules), so the artifact isn't missing from the build
output — the `ServiceLoader` lookup itself comes back empty for this module's
runtime classpath under CratonVM.

## Scale

21/21 classes in `crypto/fips1402` — 100% of the module. All: `BCFIPSEcdhEsAlgorithmProviderTest`, `BCFIPSECDSACryptoProviderTest`, `FIPS1402CertificateIdentityExtractorTest`, `FIPS1402HmacTest`, `FIPS1402JWETest`, `FIPS1402JWKTest`, `FIPS1402KeyPairVerifierTest`, `FIPS1402KeystoreTypesTest`, `FIPS1402Pbkdf2PasswordPaddingTest`, `FIPS1402SecureRandomTest`, `FIPS1402SslTest`, `FIPS1402UnitTest`, `PemUtilsBCFIPSTest`, and 8 `sdjwt.*` classes.

## Open question: CratonVM bug vs. harness classpath gap

Not yet disambiguated:

1. **CratonVM ServiceLoader/resource-discovery defect** — if the harness's
   per-class classpath genuinely includes `crypto/fips1402/target/classes`
   (which it should, per `Get-ModuleClasspathEntries` in
   `run-keycloak-suite.ps1`, which always adds `<moduleRoot>/target/classes`),
   then CratonVM's classpath-resource walk (`find_all_resource_urls` /
   `ClassLoader.getResources`, see `native-builtins/src/classloader.rs`) is
   failing to surface this module's own `META-INF/services` entry to
   `ServiceLoader`.
2. **Harness classpath-assembly gap** — if the module classpath is built from
   a jar/dependency graph that doesn't include this module's *own*
   `target/classes` for some reason specific to `crypto/fips1402`'s pom
   shape, this would be a runner issue, not a VM bug.

## Next steps

1. Confirm the exact classpath entries the runner assembled for this module
   (`.suite/classpaths/crypto_fips1402.test.cp.txt`) actually include
   `crypto/fips1402/target/classes`.
2. If present, write a minimal repro: a standalone class on that exact
   classpath doing `ServiceLoader.load(CryptoProvider.class).iterator().hasNext()`
   to confirm/deny the ServiceLoader gap in isolation from the rest of
   Keycloak's crypto bootstrap.
3. Compare against `crypto/default`, which is part of the already-passing
   238-class baseline — if `crypto/default`'s `CryptoProvider` service
   resolves fine there but `crypto/fips1402`'s doesn't under the same
   mechanism, that narrows the difference to something specific to how the
   FIPS module's jar/classes are laid out or resolved.

## Evidence

`/data/wt-keycloak-full-20260704/apps/keycloak-suite-runner/.suite/results/kcfull-others-1124-20260704-v2/others-jit/logs/crypto_fips1402.*.out.log` (21 files, identical `CryptoInitRule.before` failure signature).
