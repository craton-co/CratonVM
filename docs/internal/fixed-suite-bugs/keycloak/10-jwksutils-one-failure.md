# 10 — DefaultCryptoJWKSUtilsTest: `publicRs256` wrong JWK count

**Status:** open (crypto / JWK)
**Affected:** DefaultCryptoJWKSUtilsTest (`publicRs256`, 1/3)
**Symptom:** `java.lang.AssertionError: expected:<5> but was:<2>`

## Analysis
The test builds a JWKS (JWK set) and asserts 5 members are present; CratonVM produces
only 2. The under-count points at RSA JWK construction/serialization silently dropping
members — same RSA-JWK family as reports 06/07. Re-test after the RSA `KeyFactory` /
`RSAPublicKeySpec` work; if still short, dump the produced JWKS to see which members are
missing.
