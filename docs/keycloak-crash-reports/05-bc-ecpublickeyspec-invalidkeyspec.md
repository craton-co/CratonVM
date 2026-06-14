# 05 — BC `ECPublicKeySpec` not supported by `KeyFactory.generatePublic`

**Status:** open (crypto-provider gap)
**Affected:** BCECDSACryptoProviderTest (`getPublicFromPrivate[*]`, 3 failures)
**Symptom:**
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
