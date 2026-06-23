# 06 — KeyPairVerifier: "Keys don't match" / "Failed to decode private key"

**Status:** open (crypto)
**Affected:** DefaultCryptoKeyPairVerifierTest (4 failures)
**Symptoms:**
- `verifyWith1024PrivateKeyInPKCS8Format` → `VerificationException: Keys don't match`
- `verifyWith2048PrivateKeyInTraditionalRSAFormat` → `VerificationException: Failed to decode private key`

## Analysis
The verifier decodes RSA private keys in PKCS#8 and traditional (PKCS#1) PEM encodings
and checks the derived public key matches a reference. CratonVM either mis-decodes the
traditional RSA (PKCS#1) private key (decode failure) or derives a non-matching public
key (PKCS#8 path). Likely the same family as the RSA `KeyFactory`/`RSAPublicKeySpec`
gaps noted in memory (`reference_keycloak_suite_vs_hotspot`). Needs an isolated probe of
`KeyFactory("RSA").generatePrivate(PKCS8/PKCS1 spec)` and the public-from-private
derivation under CratonVM vs HotSpot.
