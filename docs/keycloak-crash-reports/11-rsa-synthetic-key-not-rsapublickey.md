# 11 — Synthetic RSA keys are not real `RSAPublicKey`/`RSAPrivateKey` (umbrella root cause)

**Status:** FIXED (key-type) — commit `4b2560a0` (+ the lib.rs/crypto_impl/signature
checkpoint just before it). `DefaultCryptoRSAVerifierTest` FAIL→PASS, 0 regressions.
A few *separate* follow-on bugs keep the other classes red (see "Remaining" below).
**Affected (directly or downstream):** DefaultCryptoRSAVerifierTest ✓, DefaultCryptoJWKTest,
DefaultCryptoKeyPairVerifierTest, DefaultCryptoSdJwtVPVerificationTest,
DefaultCryptoJWKSUtilsTest, PemUtilsBCTest

## Common symptoms
- `java.lang.ClassCastException: java/security/PublicKey cannot be cast to java/security/interfaces/RSAPublicKey`
- `RuntimeException: Error creating X509v3Certificate.` → `NullPointerException: Cannot invoke getAlgorithm on null`
- `PemException: unknown object in getInstance: org.bouncycastle.asn1.ASN1Integer`
- `InvalidKeySpecException: Unable to decode the private key with supported algorithms: RSA, EC`
- `VerificationException: Keys don't match` / `Failed to decode private key`
- JWK/JWKS: "Unsupported or invalid JWK", wrong member count

## Root cause
CratonVM's synthetic `KeyPairGenerator`/`KeyFactory` for **RSA** hand out bare
`java.security.PublicKey`/`PrivateKey` objects that do **not** implement the concrete
`java.security.interfaces.RSAPublicKey`/`RSAPrivateKey` (no `getModulus()`/
`getPublicExponent()` behaviour), and don't carry real PKCS#1/PKCS#8 encodings. Keycloak
and BouncyCastle then:
- cast the key to `RSAPublicKey` → **CCE**;
- read `key.getAlgorithm()` while building a cert → **NPE** on null;
- re-encode/decode the key via BC ASN.1 → "unknown object … ASN1Integer" / decode failures.

This is the same synthetic-key shadowing described in memory
(`reference_keycloak_suite_vs_hotspot`, `reference_jca_synthetic_crypto_layers`): the
synthetic natives shadow the real provider even when BouncyCastle/SunRsaSign are on the
classpath.

## Implemented solution — `route_rsa_to_real()` (the synthetic RSA was an *optimisation*, not a stub)
The synthetic path generates **real key material** with the fast Rust `crypto_impl::Rsa`
(no slow interpreter prime generation — the dominant RSA cost) and signs/verifies via a
`crypto_impl` `key_id` (no slow interpreter BigInteger modexp). Only the *wrapper type*
was wrong. The fix keeps BOTH fast paths and corrects the type:

- **Keygen stays fast Rust.** `KeyPairGenerator.generateKeyPair()` still calls
  `crypto_impl::Rsa::generate_keypair`, then re-imports the resulting components via
  `RSAPublicKeySpec(n,e)` / `RSAPrivateKeySpec(n,d)` → the real `RSAKeyFactory$Legacy`
  SPI, yielding genuine `sun.security.rsa.RSAPublic/PrivateKeyImpl` assembled into a real
  `java.security.KeyPair`. (`crypto_impl`'s RSA holds `{n,e,d}` with no CRT primes, so the
  component specs — not a PKCS#8 DER — are the right vehicle.)
- **Sign/verify stay fast Rust.** Real keys have no synthetic `key_id` slot, so they're
  bridged to their `crypto_impl` `key_id` via a GC-stable `identityHashCode` map
  (`crypto_impl::rsa_realkey_map_*`), consulted first in
  `signature::extract_key_id_from_key`.
- **Toggle.** `route_rsa_to_real()` defaults ON; `CRATONVM_SYNTHETIC_RSA=1` restores the
  bare-interface synthetic keys (faster object alloc, but the cast/cert paths fail) for
  debugging / regression bisecting. Mirrors `route_ec_to_real` / `route_pqc_to_real`.

Files: `native-builtins/src/lib.rs` (`route_rsa_to_real`),
`native-builtins/src/crypto_impl.rs` (`rsa_realkey_map_*`),
`native-builtins/src/jca/key_factory.rs` (keygen + `generatePublic` + `KeyPair` assembly +
`keypair_get_*` gate), `native-builtins/src/jca/signature.rs` (identity bridge).

Verified: real `RSAPublicKeyImpl`, `getModulus()`/`getEncoded()` byte-match HotSpot,
sign+verify pass, toggle restores synthetic.

## Remaining (SEPARATE bugs, not the key type — still red)
- **`X509Certificate.getPublicKey()` returns null** → `caCert.getPublicKey().getAlgorithm()`
  NPE in `BCCertificateUtilsProvider.generateV3Certificate` (line 134). Affects **EC and
  RSA** cert generation alike (DefaultCryptoJWKTest cert tests). A cert-object bug, not a
  key bug.
- **`KeyFactory.generatePublic(RSAPublicKeySpec)` import not handled** (only X509 DER is) →
  `publicRs256` "cannot generate a usable RSA public key from the given KeySpec". Route the
  caller's spec straight through the real KeyFactory + register material via the real key's
  `getEncoded()`.
- **No `crypto_impl` private-key DER parser** → imported RSA private keys
  (DefaultCryptoKeyPairVerifierTest) can't be bridged for fast sign; needs a PKCS#8/PKCS#1
  parser or a real RSA `Signature` SPI drive.
