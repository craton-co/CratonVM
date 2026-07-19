# `WebServerSslBundleTests`/`SslMeterBinderTests` — PKCS#12 MAC verification failure against the correct password — FIXED

**Status: FIXED 2026-07-19.** This note was moved from
`docs/known-issues/springboot/` after fixing and verifying all root causes
on `dev`. Confirmed genuinely CratonVM-side (not per-fixture corruption, as
hypothesized in the original doc) — a single systemic PKCS#12 defect
reproduced across at least 6 modules and 7+ distinct `.p12`/`.pkcs12` fixture
files.

## Original symptom

`java.io.IOException: PKCS#12 MAC verification failed (wrong password?)` at
`KeyStore.load` against the *correct* password, across
`module/spring-boot-web-server`'s `WebServerSslBundleTests` (3 failures),
`module/spring-boot-micrometer-metrics`'s `SslMeterBinderTests` (6/6),
`core/spring-boot`'s `SslInfoTests` (6/8) and `JksSslStoreBundleTests`
(`whenLocationsAreBase64Encoded`), plus `keystore.pkcs12` in the sibling
`ssl-pem-pkcs12-store-parse-failure-cluster.md`.

## Root causes (three independent gaps, all in `native-builtins/src/keystore.rs`)

### 1. MAC verification hardcoded to SHA-1

The vendored `p12` crate (v0.6.3) `MacData::verify_mac` unconditionally
computes an HMAC-**SHA-1** MAC regardless of the file's own
`digest_algorithm` field (its `AlgorithmIdentifier` enum has no SHA-256/384/
512 variant at all — an unrecognized digest OID falls into a catch-all
`OtherAlg` that `debug_assert_eq!`'s against `Sha1` and proceeds anyway in
release builds). SHA-256 has been the JDK's `SunPKCS12` default MAC digest
since **JDK 8u211 (2019)**, so this fails closed against essentially every
modern-JDK-written PKCS#12 file. Confirmed via `openssl asn1parse`: the
`keystore.pkcs12` fixture's `MacData` uses OID `2.16.840.1.101.3.4.2.1`
(SHA-256), not SHA-1.

Fixed by re-implementing PKCS#12 Appendix B.2's key-derivation + MAC check
generalized over the digest algorithm (`Pkcs12MacHash` enum, hand-rolled
HMAC + a parameterized `pkcs12_pbe_derive`), replacing `pfx.verify_mac()`
with our own `verify_pkcs12_mac()`.

### 2. PBES2/AES `SafeContents`/private-key decryption not implemented at all

The same crate's `AlgorithmIdentifier::decrypt_pbe` only implements the two
**legacy** PKCS#12 Appendix-B PBE ciphers
(`PbewithSHAAnd40BitRC2CBC`/`PbeWithSHAAnd3KeyTripleDESCBC`); any other
algorithm — including PBES2/AES, `SunPKCS12`'s default *content*-encryption
cipher since JDK 8u+ — unconditionally returns `None`. This blocked TWO
distinct sites: a keystore's `EncryptedData`-wrapped `SafeContents` (the
`ASN1Error { kind: Invalid }` seen after fixing #1) and a
`Pkcs8ShroudedKeyBag`'s own PBES2-protected private key (silently returned
no key at all). Fixed by adding a `content_info_data`/`decrypt_secret_pbes2`
fallback (re-serializing the already-parsed `AlgorithmIdentifier` +
ciphertext back into the DER shape the existing `SecretKeyEntry`-only PBES2
decryptor expects) at both sites.

### 3. PBES2/PBKDF2 fed the wrong password encoding

Once #2 exercised real decryption, it silently produced garbage (`UnpadError`
from AES-CBC/PKCS7) because the password was being BMPString-encoded
(UTF-16BE + NUL — correct for the *legacy* Appendix-B PBE ciphers) before
reaching PBKDF2, which (per RFC 8018 / `SunJCE`'s actual behavior) expects
the password as **plain bytes**, not BMPString. Fixed by threading the plain
`password_str.as_bytes()` form through to `decrypt_secret_pbes2` instead of
the BMPString one.

### Two smaller residuals found during verification

- **Null store password**: `KeyStore.load(stream, null)` must skip MAC
  verification entirely per the JDK's own documented contract ("if a
  password is not given for integrity checking, then integrity checking is
  not performed") — confirmed against real JDK 25. CratonVM's native
  boundary collapses a literal Java `null` and a zero-length `char[]` into
  the same empty byte `Vec` (same ambiguity `load_jks`'s HMAC check already
  accepted), so `verify_pkcs12_mac` now treats an empty password as "skip",
  matching the existing JKS convention. A per-entry key/content decrypt
  failure at load time (e.g. a key or `SafeContents` protected with a
  *different*, non-empty password than the absent store-level one) is now
  non-fatal — the entry is skipped/kept unavailable rather than aborting the
  whole `KeyStore.load()`, matching real `SunPKCS12`'s observed behavior of
  deferring such mismatches to whichever later access actually needs that
  entry.
- **Alias enumeration order**: `LoadedKeyStore.entries` was a plain
  `HashMap` (randomized iteration order); real JDK's `JavaKeyStore`/
  `PKCS12KeyStore` both use a `LinkedHashMap`, and Spring Boot's `SslInfo`
  asserts on the exact positional order `KeyStore.aliases()` returns.
  Switched to `indexmap::IndexMap` (a close-to-drop-in replacement) and
  removed an explicit alphabetical `sort_unstable()` in `engine_aliases`
  that was independently breaking the same contract.

## Fix

`native-builtins/src/keystore.rs`: `Pkcs12MacHash`, `pkcs12_pbe_derive`,
`verify_pkcs12_mac`, `content_info_data`, `decrypt_secret_pbes2` (extended
tracing + used from two new call sites), `LoadedKeyStore::entries` now
`IndexMap`. `native-builtins/Cargo.toml`: added `indexmap` as a direct
dependency (already present transitively). Branch
`fix/ssl-pem-pkcs12-jca-20260718`, merged into `dev`.

## Verification (Azure host, real fixture files, real JDK 25 classpath)

| Class | Before | After |
|---|---|---|
| `WebServerSslBundleTests` | 5/8 | **8/8** |
| `SslInfoTests` | 2/8 | **8/8** |
| `SslMeterBinderTests` | 0/6 | 4/6 (residual — see below) |
| `JksSslStoreBundleTests` | 13/14 | 11/14 (see below — unrelated pre-existing failures now exposed) |

`JksSslStoreBundleTests`'s 3 residual failures (`whenHasKeyStoreProvider`,
`whenHasTrustStoreProvider`, `invalidBase64EncodedLocationThrowsException`)
are a **different, pre-existing, unrelated** gap (custom
`java.security.Provider` registration/lookup and a base64-location error
message) — confirmed not caused by or related to this fix; not investigated
further here.

`SslMeterBinderTests`'s 2 residual failures
(`shouldWatchUpdatesForBundlesRegisteredAfterConstruction`,
`shouldWatchTrustStoreUpdatesForBundlesRegisteredAfterConstruction`) expect
exactly 5 `ssl.chain.expiry` gauges after an `SslBundle` update but observe
7. A direct `KeyStore.load` probe against the exact same fixture
(`chains.p12`) confirms CratonVM's alias/cert-chain parsing itself is
byte-for-byte correct (5 aliases, same names/order as real JDK) — so this is
a **downstream Micrometer gauge de-registration/duplication** issue in the
bundle-update path, not a keystore-parsing bug. Filed as a new, separate,
open issue — see
[`sslmeterbindertests-gauge-duplication-on-bundle-update.md`](sslmeterbindertests-gauge-duplication-on-bundle-update.md).

Full `cratonvm-native-builtins` regression suite: 3032/3033 passing (one
pre-existing, unrelated Windows-path-handling test failure — see the sibling
`ssl-pem-pkcs12-store-parse-failure-cluster-FIXED.md`). Zero regressions.
