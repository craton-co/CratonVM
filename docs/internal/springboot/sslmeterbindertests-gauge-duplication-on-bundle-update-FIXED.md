# `SslMeterBinderTests` — gauge count doubles after `SslBundle` update (7 vs expected 5) — FIXED

**Status: FIXED 2026-07-19.** Root cause: a PKCS#12 chain-construction bug
in `native-builtins/src/keystore.rs`, not a Micrometer/`SslMeterBinder` gap
as originally hypothesized.

## Symptom

`module/spring-boot-micrometer-metrics`'s `SslMeterBinderTests` failed 2/6:
`shouldWatchUpdatesForBundlesRegisteredAfterConstruction`,
`shouldWatchTrustStoreUpdatesForBundlesRegisteredAfterConstruction`.

```
java.lang.AssertionError:
Expected size: 5 but was: 7 in:
[io.micrometer.core.instrument.internal.DefaultGauge@..., ...]
```

## Root cause (confirmed via direct reproduction)

The original investigation's "byte-for-byte correct" `KeyStore` probe used a
**null** load password (`ks.load(stream, null)`), which happens to sidestep
this bug. The actual test path (`JksSslStoreDetails.forLocation(...)
.withPassword("secret")`) always supplies a real password — and `chains.p12`'s
private keys are *not* actually encrypted with `"secret"` (confirmed: real
JDK 25 also throws `UnrecoverableKeyException: Given final block not
properly padded` decrypting them with that password). Real JDK doesn't need
to decrypt a key to structurally register it, though: `KeyStore.aliases()`/
`isKeyEntry()`/`getCertificateChain()` all work from the PKCS#12 bag
structure alone, independent of whether the key bag itself can be decrypted.

`load_pkcs12_ex`'s `Pkcs8ShroudedKeyBag` handling did not have that
property: when decryption failed, the code logged a debug line and just
**dropped the bag out of `keys_by_local_id` entirely** (never inserted a
placeholder). The `PrivateKeyEntry` therefore never got created — its
cert(s) fell through to the "any cert-bags that didn't pair with a key"
flush loop, which registers **every individual certificate as its own
`TrustedCert` alias**. On `chains.p12` this produced the 5 correct-looking
leaf aliases (`ca`/`intermediary`/`server`/`expired`/`not-yet-valid`, each
now a bare cert instead of a `PrivateKeyEntry`) *plus* 2 more spurious
aliases (`CN=ca`/`CN=intermediary`) for the issuer certificates that
`server`'s and `intermediary`'s real chains should have included — 7 total,
matching the reported count exactly.

Separately, even the correctly-classified entries had truncated chains:
`load_pkcs12_ex` only ever paired a key with cert bags sharing its exact
PKCS#12 `localKeyId` (typically just the leaf cert). Real `SunPKCS12`
extends the chain further by X.509 issuer/subject DN matching (walk from
the leaf's issuer to another loaded cert's subject, repeat) — this codebase
had no equivalent, so even a *successfully*-paired entry's chain stopped at
the leaf (`server`: 1 cert here vs. 3 on real JDK; `intermediary`: 1 vs. 2).

## Fix (`native-builtins/src/keystore.rs`)

1. `Pkcs8ShroudedKeyBag` decrypt failure no longer drops the entry: it's
   kept in `keys_by_local_id` with the still-encrypted
   `EncryptedPrivateKeyInfo` DER (re-serialized via its own `write()`) as a
   placeholder `key_der` — mirroring the JKS loader's existing
   `jks_recover_key(...).unwrap_or(enc_key)` pattern. Alias/chain pairing
   then proceeds identically to a successful decrypt; `getKey()` on such an
   entry still won't return a usable key (matching real JDK's own
   `UnrecoverableKeyException` for a wrong password), but structural
   queries (`isKeyEntry`, `getCertificateChain`, `aliases`) now match.
2. New `extend_chain_by_issuer` helper: after the existing `localKeyId`-based
   pairing, repeatedly extends the chain by X.509 issuer/subject DN matching
   against the remaining loose cert bags (`certs_by_local_id`/
   `orphan_certs`), removing each matched cert from its source pool so it
   ends up in exactly one chain instead of also being flushed later as a
   standalone alias. Capped at 16 hops (real chains are a handful of certs
   deep; guards a malformed/cyclic file, not a realistic case).

Verified against real JDK 25 with a standalone repro
(`DefaultSslBundleRegistry` + `JksSslStoreDetails` + `SslInfo`, the actual
production code path, not just a raw `KeyStore` probe): CratonVM now reports
the identical 5 aliases with the identical chain lengths (`ca`=1,
`intermediary`=2, `server`=3, `expired`=1, `not-yet-valid`=1) as HotSpot.
`SslMeterBinderTests` now passes 6/6. No regressions: `keystore::` unit
tests (16/16) and every other SSL-bundle test class touched by today's
concurrent PKCS12 work (`SslInfoTests` 8/8, `SslAutoConfigurationTests`
4/4, `PropertiesSslBundleTests` 6/6, `SslPropertiesBundleRegistrarTests`
7/7, `WebServerSslBundleTests` 8/8) still pass.
`JksSslStoreBundleTests` still shows its own separately-filed,
already-known 3/14 residual (`whenHasKeyStoreProvider`/
`whenHasTrustStoreProvider`/`invalidBase64EncodedLocationThrowsException` —
custom `java.security.Provider` registration, an unrelated subsystem;
confirmed pre-existing by reverting this fix and re-running).

## Affected classes

| Module | Class | Methods |
|---|---|---|
| `module/spring-boot-micrometer-metrics` | `org.springframework.boot.micrometer.metrics.autoconfigure.ssl.SslMeterBinderTests` | `shouldWatchUpdatesForBundlesRegisteredAfterConstruction`, `shouldWatchTrustStoreUpdatesForBundlesRegisteredAfterConstruction` |
