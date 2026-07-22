# `spring-boot-autoconfigure` SSL bundle tests: PEM private-key parse and PKCS12 keystore load — FIXED

**Status: FIXED 2026-07-19.** This note was moved from `docs/known-issues/springboot/`
after fixing and verifying all root causes on `dev`.

## Original symptom

3 classes / 4 test methods failed loading real key/trust material from
classpath test fixtures under `org/springframework/boot/autoconfigure/ssl/`:
`PropertiesSslBundleTests.{jksPropertiesAreMappedToSslBundle,
pemPropertiesAreMappedToSslBundle}`, `SslAutoConfigurationTests
.sslBundlesCreatedWithCertificates`, `SslPropertiesBundleRegistrarTests
.shouldUseResourceLoader`. A same-day large-batch triage pass additionally
found the identical `PemPrivateKeyParser.parse` call site failing 12/47 tests
in `core/spring-boot`'s own `PemPrivateKeyParserTests` and 1/11 in
`PemSslStoreBundleTests`, plus 2/8 in `module/spring-boot-tomcat`'s
`SslConnectorCustomizerTests` (later found to be an unrelated bug — see
"What turned out NOT to be this bug" below).

## Root causes (three independent gaps, all in the JCA/keystore layer)

### 1. Generic `KeyFactory` names ("XDH"/"EdDSA") not curve-aware

`PemPrivateKeyParser` always requests the JDK's *generic* algorithm name
(`"XDH"` for both X25519/X448, `"EdDSA"` for both Ed25519/Ed448) — never the
curve-specific name — relying on the real `KeyFactory` SPI to sniff the true
curve from the key's own embedded `AlgorithmIdentifier` OID.
`native-builtins/src/jca/key_factory.rs`'s `KeyFactory` either left `"XDH"`
completely unmapped, or silently misrouted generic `"EdDSA"` to Ed25519
regardless of the key's real curve — so every Ed448/X448/X25519 PEM private
key failed, and Ed25519 keys with generic-named requests were structurally
unverified. Fixed by adding `ALGO_XDH_GENERIC`/`ALGO_EDDSA_GENERIC` sentinel
indices plus `resolve_curve_algo`, which sniffs the spec's own DER
`AlgorithmIdentifier` OID (mirroring what real JDK's non-nested
`sun.security.ec.XDHKeyFactory`/`ed.EdDSAKeyFactory` do internally) and
routes to the correct concrete SunEC SPI. `kf_generate_private` previously had
**no EdDSA/XDH route at all** (only `kf_generate_public` did) — added.

### 2. `RSASSA-PSS` `KeyFactory` collapsed onto the permissive `"RSA"` route

Real `sun.security.rsa.RSAKeyFactory$Legacy` (driven for `"RSA"`) *rejects* a
PKCS#8 key whose `AlgorithmIdentifier` OID is `id-RSASSA-PSS`
(`InvalidKeyException: Expected a RSA key, but got RSASSA-PSS`, confirmed
against real JDK 25). `PemPrivateKeyParser`'s per-algorithm fallback loop
retries with the literal name `"RSASSA-PSS"` after the `"RSA"`-named attempt
throws — but CratonVM's `algo_idx` intentionally collapses `"RSASSA-PSS"`
onto `ALGO_RSA` for `KeyPairGenerator` (correctly — the PSS choice belongs to
`Signature`, not keygen), and `KeyFactory` inherited that same collapse,
hitting the identical `$Legacy` rejection twice. Fixed with a
`KeyFactory`-only `kf_algo_idx` resolver (`ALGO_RSASSA_PSS`, distinct from
the shared `algo_idx` `KeyPairGenerator` uses) routing to the real, stricter
`sun.security.rsa.RSAKeyFactory$PSS`.

### 3. PBES2 `SecretKeyFactory`/`AlgorithmParameters` gap for encrypted PEM keys

`AlgorithmParameters.getInstance("PBES2")` (the *generic* PBES2 entry,
`com.sun.crypto.provider.PBES2Parameters$General`) was never registered —
only the specific `PBEWithHmac*AndAES_*` variants were. `sun.security.x509
.AlgorithmId.decodeParams()` resolves an encrypted PKCS#8 key's outer
params via the literal name resolved from the OID (`"PBES2"`), so this
silently failed and `EncryptedPrivateKeyInfo.getAlgParameters()` returned
null — `PemPrivateKeyParser` then fell back to the bare string `"PBES2"`
instead of `algParameters.toString()`'s real `"PBEWithHmacSHA256AndAES_256"`-
shaped name, so `SecretKeyFactory.getInstance("PBES2")` threw for **every**
encrypted PEM key regardless of PRF/cipher. Fixed by adding the missing
`AlgorithmParameters "PBES2" -> PBES2Parameters$General` service entry in
`native-builtins/src/jca/provider_chain.rs::seed_sunjce_pbe_services`.

## What turned out NOT to be this bug

`module/spring-boot-tomcat`'s `SslConnectorCustomizerTests` (2/8 failures,
originally filed here as "corroborating evidence" for the same root cause)
is a **different, unrelated, pre-existing** limitation: rustls (CratonVM's
TLS backend) never implements CBC-mode cipher suites by design, and the two
failing tests request `TLS_ECDHE_RSA_WITH_AES_128_CBC_SHA256` explicitly.
Root-caused via a from-scratch reproduction of Tomcat's exact
`SSLUtilBase.getKeyManagers()` sequence (`KeyStore`/`KeyManagerFactory`/
`SSLContext` all worked correctly in isolation) down to the real exception
Tomcat's own logging swallows:
`IllegalArgumentException: None of the [ciphers] specified are supported by
the SSL engine`. See
[`rustls-cbc-cipher-suites-not-supported.md`](rustls-cbc-cipher-suites-not-supported.md)
(new doc, filed separately — not fixed here).

## Fix

- `native-builtins/src/jca/key_factory.rs`: added `ALGO_X25519`/`ALGO_X448`/
  `ALGO_XDH_GENERIC`/`ALGO_EDDSA_GENERIC`/`ALGO_RSASSA_PSS`, `kf_algo_idx`,
  `xdh_keyfactory_spi_class`, `der_spec_algorithm_oid`/`resolve_curve_algo`,
  `drive_eddsa_or_xdh_keyfactory`, `drive_real_rsa_pss_keyfactory`; wired into
  both `kf_generate_public` and (newly) `kf_generate_private`.
- `native-builtins/src/jca/provider_chain.rs::seed_sunjce_pbe_services`:
  added the generic `AlgorithmParameters "PBES2"` service entry.

Branch `fix/ssl-pem-pkcs12-jca-20260718`, merged into `dev`.

## Verification (Azure host, real fixture files, real JDK 25 classpath)

All previously-failing classes now pass in full against the real Spring Boot
4.1.0-SNAPSHOT test suite (`SbRunner`, JUnit Platform launcher):

| Class | Before | After |
|---|---|---|
| `PropertiesSslBundleTests` | 4/6 | **6/6** |
| `SslAutoConfigurationTests` | 3/4 | **4/4** |
| `SslPropertiesBundleRegistrarTests` | 6/7 | **7/7** |
| `PemPrivateKeyParserTests` | 35/47 | **47/47** |
| `PemSslStoreBundleTests` | 10/11 | **11/11** |

Also individually verified each fixture (`x25519.key`, `x448.key`,
`ed25519.key`, `ed448.key`, `rsa-pss.key`, and their `-aes-256-cbc`/
`-aes-128-cbc` encrypted variants) against a direct probe, output classes
matching real JDK exactly (`sun.security.ec.XDHPrivateKeyImpl`,
`sun.security.ec.ed.EdDSAPrivateKeyImpl`, `sun.security.rsa
.RSAPrivateCrtKeyImpl`/`"RSASSA-PSS"`).

Full `cratonvm-native-builtins` regression suite: 3039/3041 passing after
merging `origin/dev`; the two failures
(`p57_win_path_tests::trailing_separator_is_removed_only_from_non_roots`,
`lang_class::get_constructors_returns_only_complete_public_constructor_mirrors`)
are both pre-existing and unrelated (a Windows-path-semantics test on this
Linux build host, and a reflection-metadata test from an unrelated concurrent
`dev` commit) — confirmed by checking each file's own git history, neither
touched by this fix. Zero regressions from this fix.

The PKCS12 MAC/content-decryption sub-cluster this doc's "Corroborating
evidence" section pointed at (`keystore.pkcs12`) is the SAME bug as
`webserversslbundletests-pkcs12-mac-verification-failure.md`, which a
**concurrent session independently found and fixed** (`dev` commit
`156ebfa4c`, merged before this branch — see that doc, also in `internal/`,
for its root causes and fix). This branch's own PKCS12-keystore-level PBES2
work was superseded by that commit during the `origin/dev` merge and
dropped in favor of it (equivalent fix, already reviewed and landed); this
doc's three root causes above (KeyFactory + the PEM-*private-key* PBES2
`SecretKeyFactory`/`AlgorithmParameters` gap, a distinct JCA-layer mechanism
from the PKCS12-*keystore* PBES2 decrypt the other commit addresses) remain
this branch's own, non-overlapping contribution.
