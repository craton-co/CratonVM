# 07 — SD-JWT VP: "Could not process cnf/jwk" → "Unsupported or invalid JWK"

**Status:** ✅ FIXED — `DefaultCryptoSdJwtVPVerificationTest` now `OK (24 tests)` in BOTH
default and `CRATONVM_SYNTHETIC_RSA=1` modes; 0 regressions.

## Fix (two layers)
1. **JWK → key parse.** `JWKParser.createRSAPublicKey` does
   `KeyFactory.getInstance("RSA").generatePublic(new RSAPublicKeySpec(n, e))`. In default
   mode this is driven through the real `RSAKeyFactory$Legacy` (already in-tree). In
   synthetic mode (`CRATONVM_SYNTHETIC_RSA=1`) the spec path now reads the spec's
   `getModulus()`/`getPublicExponent()` and builds a synthetic key backed by a
   `crypto_impl` `key_id` (`jca/key_factory.rs::rsa_pubspec_components`) — previously
   only X509-DER specs were handled, so the spec dead-ended in "Unsupported or invalid
   JWK".
2. **Key-binding JWT verify.** The `__CnfRSA` case verifies a holder JWT signed RS256
   **and** PS256/PS384/PS512. RS256 already worked; RSA-PSS did not. Added
   RSASSA-PSS verification (`crypto_impl::rsa_verify_pss`, RFC 8017 §9.1.2 EMSA-PSS-VERIFY,
   salt len = hash len) and mapped keycloak's `SHA{256,384,512}withRSAandMGF1` names to it
   in `jca/signature.rs`. Validated against HotSpot-produced PS256/384/512 vectors.

(historical) **Original symptom:**
```
org.keycloak.common.VerificationException: Could not process cnf/jwk
Caused by: java.lang.IllegalArgumentException: Unsupported or invalid JWK
```

## Analysis
The key-binding JWT carries a confirmation key (`cnf.jwk`) as an RSA JWK. Keycloak parses
it into a `PublicKey`. The "Unsupported or invalid JWK" comes from JWK→key conversion
rejecting the RSA JWK — again consistent with the RSA `KeyFactory`/`RSAPublicKeySpec`
shortfall (see report 06 and memory). EC-`cnf` SD-JWT VP cases pass; only RSA-`cnf` fail.

## Fix direction
Same root family as 06 — make RSA public-key construction from JWK params
(`RSAPublicKeySpec`) work (route to real provider or implement the spec), which should
clear the `__CnfRSA` SD-JWT VP cases too.
