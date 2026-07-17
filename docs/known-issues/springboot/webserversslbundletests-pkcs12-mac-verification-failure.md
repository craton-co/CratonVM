# `WebServerSslBundleTests`/`SslMeterBinderTests` — PKCS#12 MAC verification failure against the correct password

**Status: OPEN — found 2026-07-17**

## Update 2026-07-17 (same-day, second module)

The identical exact error string (`PKCS#12 MAC verification failed (wrong
password?)`) independently reproduces in **`module/spring-boot-micrometer-metrics`**,
class `org.springframework.boot.micrometer.metrics.autoconfigure.ssl.SslMeterBinderTests`
— 6/6 tests fail, all loading `classpath:certificates/chains.p12` /
`classpath:certificates/chains2.p12` (different fixture files than
`WebServerSslBundleTests`' `test.p12`, and different from
`ssl-pem-pkcs12-store-parse-failure-cluster.md`'s `keystore.pkcs12` — a
*third* distinct PKCS12 fixture file, same failure mode). Representative
trace:

```
=> java.lang.IllegalStateException: Unable to create key store: Could not load store from 'classpath:certificates/chains2.p12'
   org.springframework.boot.ssl.jks.JksSslStoreBundle.createKeyStore(JksSslStoreBundle.java:114)
   org.springframework.boot.ssl.jks.JksSslStoreBundle.lambda$new$0(JksSslStoreBundle.java:77)
   org.springframework.util.function.SingletonSupplier.get(SingletonSupplier.java:113)
   org.springframework.boot.ssl.jks.JksSslStoreBundle.getKeyStore(JksSslStoreBundle.java:83)
   org.springframework.boot.info.SslInfo$BundleInfo.<init>(SslInfo.java:111)
   org.springframework.boot.micrometer.metrics.autoconfigure.ssl.SslMeterBinder.bindTo(SslMeterBinder.java:96)
 Caused by: java.lang.IllegalStateException: Could not load store from 'classpath:certificates/chains2.p12'
 Caused by: java.io.IOException: PKCS#12 MAC verification failed (wrong password?)
   java.security.KeyStore.load(KeyStore.java:1522)
```

Full log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard4/logs/module_spring-boot-micrometer-metrics.org.springframework.boot.micrometer.metrics.autoconfigur-a08e64221df1.out.log`

This is now the **third** independently-filed same-day occurrence of this
exact error string across three different modules and three different
`.p12` fixture files (`test.p12` here, `keystore.pkcs12` in
`ssl-pem-pkcs12-store-parse-failure-cluster.md`, `chains.p12`/`chains2.p12`
in `spring-boot-micrometer-metrics`) — strong evidence this is a genuine,
general CratonVM PKCS12 `KeyStore.load` MAC-verification defect, not a
per-fixture data-corruption issue (hypothesis #2 in the original Root cause
section below becomes less likely the more independent fixture files
reproduce it; still not ruled out without a byte-identical-on-disk check).

## Symptom

Module `module/spring-boot-web-server`, class `WebServerSslBundleTests`, 3 failures: `whenJksKeyStoreAndPemTrustStoreProperties`, `whenPemKeyStoreAndJksTrustStoreProperties`, `whenFromJksProperties`.

```
java.io.IOException: PKCS#12 MAC verification failed (wrong password?)
	at java.security.KeyStore.load(KeyStore.java:1522)
```

reached via `JksSslStoreBundle.loadKeyStore` loading `classpath:test.p12`. The err.log independently confirms: `keystore: engineLoad: parse failed: PKCS#12 MAC verification failed (wrong password?)`.

Log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard*/logs/module_spring-boot-web-server.WebServerSslBundleTests.out.log` (lines 17-77).

**Confirmed NOT an intentional bad-password scenario** — the test source (`WebServerSslBundleTests.java:52-187`) was read; all three failing tests correctly call `ssl.setKeyStorePassword("secret")` / `ssl.setTrustStorePassword("secret")`, matching what the `@WithPackageResources("test.p12")`-provided fixture keystore is actually encrypted with. CratonVM's PKCS12 keystore loading is genuinely failing MAC verification against the *correct* password.

## Root cause

**Hypothesis, not confirmed.** Two candidate mechanisms:

1. CratonVM's PKCS12 `KeyStoreSpi`/HMAC verification implementation has a genuine bug (see [[reference_jca_synthetic_crypto_layers]] in memory — "where synthetic JCA key intrinsics live" — this crypto layer has had prior issues).
2. `@WithPackageResources` doesn't materialize `test.p12` byte-identically to the real resource on disk (a resource-copy/encoding corruption would also produce a MAC verification failure against an otherwise-correct password).

No existing doc found (`grep -rl "PKCS#12 MAC verification failed"` across `docs/internal/fixed-suite-bugs/`, `docs/internal/springboot/`, `docs/known-issues/springboot/` → zero hits). Neither hypothesis traced into CratonVM source — needs a follow-up that first checks whether the materialized `test.p12` on disk is byte-identical to the real Spring Boot test resource (ruling out #2) before assuming the JCA MAC-verification code itself is wrong.

## Update 2026-07-17 (large `core/spring-boot` batch triage, 73-class rerun doc pass)

**Fourth and fifth** independent occurrences, both in `core/spring-boot`
itself, both against the module's own `classpath:test.p12` fixture (the
same filename `WebServerSslBundleTests` above uses, but this is a
*different* file in a different module's `src/test/resources`, so it's not
literally the same bytes):

```
JUnit Jupiter:SslInfoTests:multipleBundlesShouldProvideSslInfo()
  => java.lang.IllegalStateException: Unable to create key store: Could not load store from 'classpath:test.p12'
     org.springframework.boot.ssl.jks.JksSslStoreBundle.createKeyStore(JksSslStoreBundle.java:114)
   Caused by: java.lang.IllegalStateException: Could not load store from 'classpath:test.p12'
   Caused by: java.io.IOException: PKCS#12 MAC verification failed (wrong password?)
     java.security.KeyStore.load(KeyStore.java:1522)
```

`SslInfoTests` fails this way 6/8 times total, against 4 different
`.p12`/fixture references: `classpath:test.p12` (x3: `multipleBundlesShouldProvideSslInfo`,
`bothKeyStoreAndTrustStoreCertificatesShouldProvideSslInfo`,
`trustStoreCertificatesShouldProvideSslInfo`), `classpath:test-expired.p12`
(`expiredCertificateShouldProvideSslInfo`), and (per the err.log's matching
`keystore: engineLoad: parse failed: PKCS#12 MAC verification failed
(wrong password?)` WARN lines, 6 total) 2 more fixture variants not fully
captured in the out.log excerpt read this session — all the identical
`IOException: PKCS#12 MAC verification failed (wrong password?)` at
`KeyStore.load(KeyStore.java:1522)`.

`JksSslStoreBundleTests.whenLocationsAreBase64Encoded` also fails the same
way, but through a slightly different loading path — the test reads
`classpath:test.p12` at run time and re-encodes it as a `base64:` location
before parsing:

```java
JksSslStoreDetails keyStoreDetails = JksSslStoreDetails.forLocation(encodeFileContent("classpath:test.p12"))
    .withPassword("secret");
```

so it isn't fully independent evidence against hypothesis #2 above (it
still ultimately reads the same on-disk `test.p12` first), but it does rule
out one more variant of that hypothesis: whatever's failing MAC
verification here survives a full base64 round-trip through the *test's
own* `Base64.getEncoder()`/`getDecoder()` calls, so if there were a subtle
byte-level corruption in how CratonVM materializes/copies test resources to
disk, it would have to originate no later than the very first
`classpath:test.p12` read (not from anything the base64 encode/decode step
itself does).

Full logs:
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard1/logs/core_spring-boot.org.springframework.boot.info.SslInfoTests.out.log`
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard1/logs/core_spring-boot.org.springframework.boot.ssl.jks.JksSslStoreBundleTests.out.log`

This is now the **fourth and fifth** independently-filed same-day
occurrence of this exact error string, across five different modules and
at least five different `.p12`/`.pkcs12` fixture files total
(`test.p12`/`test-expired.p12` here in `core/spring-boot`, `test.p12` in
`module/spring-boot-web-server`, `chains.p12`/`chains2.p12` in
`module/spring-boot-micrometer-metrics`, `keystore.pkcs12` in
`ssl-pem-pkcs12-store-parse-failure-cluster.md`) — further strengthening
the "genuine CratonVM `KeyStore.load`/PKCS12-MAC defect" conclusion over
any single-fixture-corruption explanation. Still not traced to a specific
file:line in CratonVM's PKCS12/JCA source this session.

## Affected classes

- `module/spring-boot-web-server` | `WebServerSslBundleTests`
- `module/spring-boot-micrometer-metrics` | `org.springframework.boot.micrometer.metrics.autoconfigure.ssl.SslMeterBinderTests` (6/6 tests, found 2026-07-17, see "Update" above)
- `core/spring-boot` | `org.springframework.boot.info.SslInfoTests` (6/8 tests, added large-batch triage)
- `core/spring-boot` | `org.springframework.boot.ssl.jks.JksSslStoreBundleTests` (`whenLocationsAreBase64Encoded`, 1 of 14 tests, added large-batch triage)
