# 05 — BC `ECPublicKeySpec` not supported by `KeyFactory.generatePublic`

**Status:** ✅ FIXED — `BCECDSACryptoProviderTest` now `OK (3 tests)` (secp256r1/384r1/521r1);
0 regressions across the crypto module (incl. JWKT cert gen+verify, ECDSA sign/verify, SdJwt).

## Fix (two parts — the spec was only half of it)
1. **BC-specific spec rejected.** `KeyFactory.getInstance("EC").generatePublic(org.
   bouncycastle.jce.spec.ECPublicKeySpec)` threw "ECPublicKeySpec not supported". `key_factory.rs`
   now falls back to BC's own `ec.KeyFactorySpi$EC.engineGenerate{Public,Private}` when SunEC
   rejects a spec (via the generic `drive_keyspec_spi`). Harmless — only fires for BC-specific
   specs SunEC can't parse.
2. **Provider-aware EC keygen (the real root cause).** keycloak's `getPublicFromPrivate`
   casts `(BCECPrivateKey) testKey.getPrivate()` + uses BC point math. But under
   `route_ec_to_real`, CratonVM's EC keygen ALWAYS produced **SunEC** keys regardless of the
   requested provider, so the BC cast/math operated on the wrong key type → wrong point. Fix:
   `kpg_get_instance` records a BouncyCastle provider request (`getInstance(alg,"BC"|BCprovider)`,
   resolved by `requested_provider_name`), and EC `generateKeyPair` then drives BC's
   `KeyPairGeneratorSpi$EC` (real `BCECPrivate/PublicKey`) instead of `sun.security.ec.
   ECKeyPairGenerator`. **Crucially, BC EC keys still sign/verify through our `Signature`
   natives** and BC's `G.multiply(d)` is correct under CratonVM (proven by probe), so
   `getPublicFromPrivate` now matches. Default (no provider / non-BC) keeps SunEC.

(historical) **Symptom:**
```
RuntimeException: Received an invalid key spec.
Caused by: java.security.spec.InvalidKeySpecException:
    org.bouncycastle.jce.spec.ECPublicKeySpec not supported.
```

## Analysis
The test derives a public key from a private key and feeds BC's
`org.bouncycastle.jce.spec.ECPublicKeySpec` (the BC-specific spec carrying an EC point +
params) to `KeyFactory("EC")`. CratonVM's synthetic EC `KeyFactory` only recognises the
standard `java.security.spec.*` specs, so the BC spec is rejected. Real BC supports it.

## Fix direction
Route EC `KeyFactory.generatePublic`/`getKeySpec` for BC-specific specs to the real BC
provider (as with other EC operations), or teach the synthetic EC KeyFactory to accept
`org.bouncycastle.jce.spec.ECPublicKeySpec`/`ECPrivateKeySpec`.
