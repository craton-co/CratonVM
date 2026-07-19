# `SslMeterBinderTests` — gauge count doubles after `SslBundle` update (7 vs expected 5)

**Status: OPEN — found 2026-07-19**, as a residual while fixing
`docs/internal/springboot/webserversslbundletests-pkcs12-mac-verification-failure-FIXED.md`.

## Symptom

`module/spring-boot-micrometer-metrics`'s `SslMeterBinderTests` fails 2/6:
`shouldWatchUpdatesForBundlesRegisteredAfterConstruction`,
`shouldWatchTrustStoreUpdatesForBundlesRegisteredAfterConstruction`.

```
java.lang.AssertionError:
Expected size: 5 but was: 7 in:
[io.micrometer.core.instrument.internal.DefaultGauge@..., ...]
```

Both tests: register a "dummy" bundle backed by `chains2.p12`, bind a
`MeterRegistry`, register a "test-0" bundle also backed by `chains2.p12`,
then call `sslBundleRegistry.updateBundle("test-0", ...)` to swap it to
`chains.p12`, and assert exactly 5 `ssl.chain.expiry` gauges tagged
`bundle=test-0` afterward. CratonVM reports 7.

## What this is NOT

A direct `KeyStore.load(stream, null)` probe against the exact
`chains.p12` fixture confirms CratonVM's PKCS#12 alias/cert-chain parsing
is byte-for-byte correct and matches real JDK 25 exactly:

```
alias count=5
alias=ca isKey=true ...
alias=intermediary isKey=true ...
alias=server isKey=true ...
alias=expired isKey=true ...
alias=not-yet-valid isKey=true ...
```

(Real JDK reports the identical 5 aliases, same order, same `isKey`/`isCert`
flags.) So this is **not** a keystore-parsing bug — the underlying
`KeyStore` this binds to already has the right 5 entries.

## Hypothesis (not yet confirmed)

The failure only appears in the two tests that call `updateBundle(...)` to
*replace* an already-registered, already-bound bundle's store — the simpler
`shouldDifferentiateKeyStoreAndTrustStoreMetrics`-style tests (register once,
bind once, no update) pass. This points at `SslMeterBinder`'s bundle-update
listener not de-registering (or replacing rather than accumulating) the
previous store's gauges before/when re-binding the new store — i.e. a
Micrometer gauge lifecycle/de-duplication gap in the update path, not
anything in `native-builtins/src/keystore.rs`. 5 (old, presumably stale)
gauges from the initial `chains2.p12` registration plus 5 new ones from the
post-update `chains.p12` store would be 10, not 7, so the exact mechanism
(which specific gauges survive/duplicate) is not yet pinned down — needs a
focused trace of `SslMeterBinder`'s bundle-registration/update listener
logic to confirm.

## Reproduction

Real Spring Boot 4.1.0-SNAPSHOT classpath, `SbRunner`:

```
module/spring-boot-micrometer-metrics: org.springframework.boot.micrometer.metrics.autoconfigure.ssl.SslMeterBinderTests
```

Fixture files: `module/spring-boot-micrometer-metrics/src/test/resources/certificates/{chains,chains2}.p12`.

## Affected classes

| Module | Class | Methods |
|---|---|---|
| `module/spring-boot-micrometer-metrics` | `org.springframework.boot.micrometer.metrics.autoconfigure.ssl.SslMeterBinderTests` | `shouldWatchUpdatesForBundlesRegisteredAfterConstruction`, `shouldWatchTrustStoreUpdatesForBundlesRegisteredAfterConstruction` |
