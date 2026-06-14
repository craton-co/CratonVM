# 05 — BC `ECPublicKeySpec` not supported by `KeyFactory.generatePublic`

**Status:** PARTIAL — the reported spec-rejection IS fixed; the test stays red on a
DEEPER, separate provider-routing issue (below).
**Affected:** BCECDSACryptoProviderTest (`getPublicFromPrivate[*]`, 3 failures)

## What's fixed
`KeyFactory.getInstance("EC").generatePublic(org.bouncycastle.jce.spec.ECPublicKeySpec)`
no longer throws "ECPublicKeySpec not supported": when SunEC rejects a BC-specific spec,
`key_factory.rs` now falls back to BC's own
`ec.KeyFactorySpi$EC.engineGeneratePublic/Private` (which accepts the BC spec). Harmless —
only triggers for BC-specific specs SunEC can't parse.

## Deeper residual (why the test is still red)
The test casts `(BCECPrivateKey) testKey.getPrivate()` and derives the public key with BC's
`getParameters().getG().multiply(getD())`. But under `route_ec_to_real` CratonVM's EC keygen
ALWAYS produces **SunEC** `EC{Public,Private}KeyImpl` (via `sun.security.ec.
ECKeyPairGenerator`), regardless of the requested BouncyCastle provider — so `testKey` is a
SunEC key and the BC-specific `getPublicFromPrivate` operates on the wrong key type / BC
point math, yielding a non-matching point (fails identically with `CRATONVM_DISABLE_JIT=1`,
so not a JIT miscompile). A real fix needs **provider-aware EC keygen** (produce a real BC
keypair when BC is requested) — a larger change to core EC routing with regression risk
across the many EC tests that currently rely on the SunEC path. Scoped out here.
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
