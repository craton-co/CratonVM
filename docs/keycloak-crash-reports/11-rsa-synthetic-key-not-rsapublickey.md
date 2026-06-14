# 11 — Synthetic RSA keys are not real `RSAPublicKey`/`RSAPrivateKey` (umbrella root cause)

**Status:** open (architectural — JCA synthetic key layer)
**Affected (directly or downstream):** DefaultCryptoRSAVerifierTest, DefaultCryptoJWKTest,
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

## Fix direction (matches existing EC / ML-DSA strategy)
Route RSA `KeyPairGenerator`/`KeyFactory`/`Signature` to the **real** provider exactly
like EC (`route_ec_to_real`) and ML-DSA/ML-KEM (`route_pqc_to_real`) already are, so the
returned keys are genuine `RSAPublicKey`/`RSAPrivateKey` with correct encodings. This is
the single highest-leverage remaining change for the keycloak crypto suite, but it is an
architectural change to the JCA layer (broad blast radius) and was scoped out of this
pass to avoid VM-wide regressions. Related smaller follow-ons gated behind it:
- report 06 (KeyPairVerifier decode), 07 (SD-JWT RSA cnf/jwk), 10 (JWKS count),
  and the BC cert `getRDNs`/CN-extraction (report 01 follow-on) likely clear once RSA
  keys are real.
