# 14 — `X509Certificate.verify()` failed: signature-algorithm OID not resolved

**Status:** FIXED — `native-builtins/src/jca/signature.rs::algo_idx`.
**Affected:** DefaultCryptoJWKTest (P-256 EC cert tests now ✓), and **any** EC/RSA
`X509Certificate.verify()` / cert-chain validation.
**Symptom:** `java.security.SignatureException: certificate does not verify with supplied key`.

## Root cause (isolated via apps/probe/kccert)
Raw ECDSA sign/verify worked, and the cert's embedded signature **verified correctly over
its TBS** when checked manually with `Signature.getInstance("SHA256withECDSA")`. The only
divergence: BC's `X509CertificateObject.verify()` resolves the verifier by the
**signature-algorithm OID** —
`Signature.getInstance(c.getSignatureAlgorithm().getAlgorithm().getId())` — i.e.
`getInstance("1.2.840.10045.4.3.2")`, not the friendly name.

`signature.rs::algo_idx` mapped only friendly names (`"SHA256WITHECDSA"`, …); the OID
forms fell through to `-1` ("unknown"), so the verifier ran with no real algorithm and
returned **false** → "certificate does not verify". (CratonVM's `getSigAlgName()` also
returns the raw OID rather than the friendly name, which is what surfaced the OID path.)

```
Signature.getInstance("SHA256withECDSA").verify(sig)    // true
Signature.getInstance("1.2.840.10045.4.3.2").verify(sig) // CratonVM: FALSE  (HotSpot: true)
```

## Fix
Add the signature-algorithm OID aliases to `algo_idx`:
- ecdsa-with-SHA256/384/512 → `1.2.840.10045.4.3.{2,3,4}`
- sha{1,256,384,512}WithRSAEncryption → `1.2.840.113549.1.1.{5,11,12,13}`
- Ed25519 → `1.3.101.112`

EC and RSA `cert.verify()` now succeed. DefaultCryptoJWKTest 4→2 failures.

## Remaining in JWKTest (separate) — P-384 / P-521 curves
`publicEs256P384` / `publicEs256P521` now fail with
`IllegalArgumentException: native EC scalar multiply failed (bad point/scalar)` —
CratonVM's native EC (SunEC routing / crypto_impl) is **P-256 only**. P-384/P-521 support
is a separate, larger gap (multi-curve EC arithmetic), unrelated to cert verification.
