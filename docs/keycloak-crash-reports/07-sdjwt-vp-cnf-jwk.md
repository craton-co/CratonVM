# 07 — SD-JWT VP: "Could not process cnf/jwk" → "Unsupported or invalid JWK"

**Status:** open (crypto / JWK parsing)
**Affected:** DefaultCryptoSdJwtVPVerificationTest (2 failures, the `__CnfRSA` cases)
**Symptom:**
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
