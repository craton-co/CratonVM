# Keycloak crypto/fips1402 CryptoProvider ServiceLoader bootstrap

Status: fixed on 2026-07-04 by making `java.security.Provider`'s raw
Map/service enumeration surfaces reflect the provider side tables populated by
`Provider.put`.

## Fix

CratonVM now registers native overrides for:

- `Provider.containsKey(Object)`
- `Provider.get(Object)`
- `Provider.getServices()`

The existing `Provider.put(Object,Object)` bridge already recorded raw provider
properties and parsed service entries into Rust side tables, but it did not
mutate the inherited `Hashtable` backing store. BouncyCastle FIPS/JSSE consults
those inherited surfaces during provider construction:

- `BouncyCastleFipsProvider` checks `containsKey("MessageDigest.SHA-1")` before
  registering aliases.
- `BouncyCastleJsseProvider` exposes `KeyManagerFactory` /
  `TrustManagerFactory` entries through provider services, and Keycloak scans
  `Provider.getServices()` while initializing FIPS TLS defaults.

Before this fix the service descriptor itself was visible, but the provider
constructor failed. `ServiceLoader` then skipped the failing provider and
Keycloak observed an empty `CryptoProvider` list.

## Validation

- Local registry test:
  `CARGO_TARGET_DIR=target-keycloak-fips-serviceloader-20260704-local cargo test -p cratonvm-vm --test wp6_5_finish_provider_chain_resolution -- --nocapture`
  - result: 4 passed.
- Azure registry test:
  `CARGO_TARGET_DIR=target-keycloak-fips-serviceloader-remote-20260704-test cargo test -p cratonvm-vm --test wp6_5_finish_provider_chain_resolution -- --nocapture`
  - result: 4 passed.
- Azure unique binary:
  `/data/cratonvm-keycloak-fips-serviceloader-remote-20260704-001/cratonvm-keycloak-fips-serviceloader-20260704`
- Direct constructor probe:
  `KcFipsProviderCtorProbe` now prints
  `constructed=org.keycloak.crypto.fips.FIPS1402Provider`.
- ServiceLoader probe on the effective `crypto/fips1402` classpath now prints:

```
resourceCount=1
provider=org.keycloak.crypto.fips.FIPS1402Provider
providerCount=1
```

- Suite-runner representative class:
  `crypto/fips1402 :: org.keycloak.crypto.fips.test.FIPS1402UnitTest`
  no longer fails in the container/bootstrap path. The original signature was
  `tests=0 ... containersFailed=1` with `Not able to load any cryptoProvider`;
  after the fix it reaches the test body with `containersFailed=0`.

## Residual

The broader `crypto/fips1402` module is not green. The representative
`FIPS1402UnitTest` now fails later in the test body:

```
java.lang.IllegalStateException: Illegal state. Please init first before obtaining provider
    at org.keycloak.common.crypto.CryptoIntegration.getProvider(CryptoIntegration.java:52)
```

That is a separate post-bootstrap Keycloak/FIPS state issue; it is not the
ServiceLoader-empty failure archived here.

---

# Historical report: crypto/fips1402 CryptoIntegration finds zero CryptoProvider services

Historical original status: open.

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
providers back. The `../../../../apps/META-INF/services/org.keycloak.common.crypto.CryptoProvider`
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
   `ClassLoader.getResources`, see `../../../../native-builtins/src/classloader.rs`) is
   failing to surface this module's own `../../../../apps/META-INF/services` entry to
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
